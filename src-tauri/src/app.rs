use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use globset::GlobSet;
use notify::RecommendedWatcher;
use tauri::Emitter;

use crate::db::{now_millis, Db, FolderRow};
use crate::model::AppSettings;
use crate::store::BlobStore;
use crate::watcher::Job;

pub struct Engine {
    pub tx: Sender<Job>,
    pub watchers: HashMap<i64, RecommendedWatcher>,
    pub failed: HashSet<i64>,
}

#[derive(Clone)]
pub struct FolderContext {
    pub row: FolderRow,
    pub root: PathBuf,
    pub patterns: Arc<GlobSet>,
}

pub struct AppState {
    pub db: Arc<Db>,
    pub store: Arc<BlobStore>,
    pub settings: RwLock<AppSettings>,
    pub engine: Mutex<Engine>,
    pub handle: Mutex<Option<tauri::AppHandle>>,
    pub storage_lock: RwLock<()>,
    folders: RwLock<HashMap<i64, FolderContext>>,
    notify_pending: AtomicBool,
    last_notify: AtomicI64,
}

impl AppState {
    pub fn new(
        db: Arc<Db>,
        store: Arc<BlobStore>,
        settings: AppSettings,
        tx: Sender<Job>,
    ) -> Arc<Self> {
        let state = Arc::new(Self {
            db,
            store,
            settings: RwLock::new(settings),
            engine: Mutex::new(Engine {
                tx,
                watchers: HashMap::new(),
                failed: HashSet::new(),
            }),
            handle: Mutex::new(None),
            storage_lock: RwLock::new(()),
            folders: RwLock::new(HashMap::new()),
            notify_pending: AtomicBool::new(false),
            last_notify: AtomicI64::new(0),
        });
        let _ = state.reload_folders();
        state
    }

    pub fn reload_folders(&self) -> Result<()> {
        let mut map = HashMap::new();
        for row in self.db.list_folder_rows()? {
            let patterns = Arc::new(crate::ingest::folder_patterns(&row.exclude_globs));
            map.insert(
                row.id,
                FolderContext {
                    root: PathBuf::from(&row.path),
                    row,
                    patterns,
                },
            );
        }
        *self.folders.write().unwrap_or_else(|e| e.into_inner()) = map;
        Ok(())
    }

    pub fn folder(&self, id: i64) -> Option<FolderContext> {
        if let Some(ctx) = self
            .folders
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
        {
            return Some(ctx.clone());
        }
        let row = self.db.folder_row(id).ok().flatten()?;
        Some(FolderContext {
            root: PathBuf::from(&row.path),
            patterns: Arc::new(crate::ingest::folder_patterns(&row.exclude_globs)),
            row,
        })
    }

    pub fn folder_for_path(&self, path: &std::path::Path) -> Option<i64> {
        let folders = self.folders.read().unwrap_or_else(|e| e.into_inner());
        folders
            .values()
            .filter(|ctx| path.starts_with(&ctx.root))
            .max_by_key(|ctx| ctx.row.path.len())
            .map(|ctx| ctx.row.id)
    }

    pub fn folder_active(&self, folder: &FolderRow) -> bool {
        folder.enabled && !folder.paused && !folder.archived && !self.paused()
    }

    pub fn notify(&self) {
        self.notify_pending.store(true, Ordering::SeqCst);
        let now = now_millis();
        if now - self.last_notify.load(Ordering::SeqCst) >= 1000 {
            self.flush_notify();
        }
    }

    pub fn flush_notify(&self) {
        if !self.notify_pending.swap(false, Ordering::SeqCst) {
            return;
        }
        self.last_notify.store(now_millis(), Ordering::SeqCst);
        self.emit("history-changed");
    }

    pub fn emit(&self, event: &str) {
        let handle = self.handle.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = handle.as_ref() {
            let _ = handle.emit(event, ());
        }
    }

    pub fn send(&self, job: Job) {
        let engine = self.engine.lock().unwrap_or_else(|e| e.into_inner());
        let _ = engine.tx.send(job);
    }

    pub fn refresh_watchers(self: &Arc<Self>) -> Result<()> {
        self.reload_folders()?;
        let folders = self.db.list_folder_rows()?;
        let mut engine = self.engine.lock().unwrap_or_else(|e| e.into_inner());
        let wanted: Vec<i64> = folders
            .iter()
            .filter(|f| f.enabled && !f.paused)
            .map(|f| f.id)
            .collect();

        engine.watchers.retain(|id, _| wanted.contains(id));
        engine.failed.retain(|id| wanted.contains(id));

        for folder in folders {
            if !folder.enabled || folder.paused || engine.watchers.contains_key(&folder.id) {
                continue;
            }
            match crate::watcher::spawn_watcher(
                folder.id,
                PathBuf::from(&folder.path),
                engine.tx.clone(),
            ) {
                Ok(watcher) => {
                    engine.watchers.insert(folder.id, watcher);
                    if engine.failed.remove(&folder.id) {
                        let _ = engine.tx.send(Job::Reconcile { folder_id: folder.id });
                    }
                    log::info!("watching {}", folder.path);
                }
                Err(err) => {
                    if engine.failed.insert(folder.id) {
                        log::warn!("cannot watch {} yet: {err:#}", folder.path);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn paused(&self) -> bool {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .paused
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        let json = {
            let mut settings = self.settings.write().unwrap_or_else(|e| e.into_inner());
            settings.paused = paused;
            settings.to_stored_json()?
        };
        self.db.set_setting("app", &json)?;
        self.emit("settings-changed");
        Ok(())
    }
}
