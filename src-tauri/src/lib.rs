pub mod app;
pub mod commands;
pub mod db;
pub mod diff;
pub mod extract;
pub mod ingest;
pub mod logging;
pub mod model;
pub mod restore;
pub mod retention;
pub mod store;
pub mod tempfiles;
pub mod watcher;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use log::LevelFilter;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, RunEvent, Wry, WindowEvent};

use crate::app::AppState;
use crate::db::Db;
use crate::logging::FileLogger;
use crate::model::AppSettings;
use crate::store::BlobStore;
use crate::watcher::Job;

struct TrayItems {
    pause: MenuItem<Wry>,
}

fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("FileHistoryTracker")
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_skip_taskbar(false);
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn hide_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
        let _ = window.set_skip_taskbar(true);
    }
}

pub fn update_tray_pause_label(app: &tauri::AppHandle, paused: bool) {
    if let Some(items) = app.try_state::<Mutex<TrayItems>>()
        && let Ok(items) = items.lock() {
            let _ = items
                .pause
                .set_text(if paused { "Resume monitoring" } else { "Pause monitoring" });
        }
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(if paused {
            "File History Tracker (paused)"
        } else {
            "File History Tracker"
        }));
    }
}

fn spawn_background<F: FnOnce() + Send + 'static>(name: &str, f: F) {
    if let Err(err) = std::thread::Builder::new().name(name.to_string()).spawn(f) {
        log::error!("cannot start {name}: {err}");
    }
}

fn build_tray(app: &tauri::AppHandle, paused: bool) -> Result<()> {
    let open = MenuItem::with_id(app, "open", "Open File History Tracker", true, None::<&str>)?;
    let pause = MenuItem::with_id(
        app,
        "pause",
        if paused { "Resume monitoring" } else { "Pause monitoring" },
        true,
        None::<&str>,
    )?;
    let snapshot = MenuItem::with_id(app, "snapshot", "Snapshot all folders now", true, None::<&str>)?;
    let cleanup = MenuItem::with_id(app, "cleanup", "Run cleanup now", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &pause, &snapshot, &cleanup, &separator, &quit])?;
    app.manage(Mutex::new(TrayItems { pause }));

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("File History Tracker")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let state = Arc::clone(app.state::<Arc<AppState>>().inner());
            match event.id.as_ref() {
                "open" => show_main_window(app),
                "pause" => {
                    let paused = !state.paused();
                    if let Err(err) = state.set_paused(paused) {
                        log::error!("cannot change pause state: {err:#}");
                        return;
                    }
                    update_tray_pause_label(app, paused);
                    if !paused {
                        for folder in state.db.list_folder_rows().unwrap_or_default() {
                            state.send(Job::Reconcile { folder_id: folder.id });
                        }
                    }
                }
                "snapshot" => {
                    for folder in state.db.list_folder_rows().unwrap_or_default() {
                        state.send(Job::Snapshot { folder_id: folder.id });
                    }
                }
                "cleanup" => spawn_background("cleanup-now", move || {
                    match retention::run_cleanup(&state, db::now_millis()) {
                        Ok(report) => log::info!(
                            "cleanup removed {} versions and {} blobs",
                            report.removed_versions,
                            report.removed_blobs
                        ),
                        Err(err) => log::error!("cleanup failed: {err:#}"),
                    }
                    state.notify();
                }),
                "quit" => app.exit(0),
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    update_tray_pause_label(app, paused);
    Ok(())
}

fn load_settings(db: &Db, data_dir: &Path) -> AppSettings {
    let stored = db.get_setting("app").ok().flatten();
    let parsed = match stored.as_deref() {
        None => Some(AppSettings::default()),
        Some(json) => match serde_json::from_str::<AppSettings>(json) {
            Ok(settings) => Some(settings),
            Err(err) => {
                log::error!("stored settings could not be read ({err}); keeping a backup and using defaults for this run");
                let _ = db.set_setting("app.bak", json);
                None
            }
        },
    };
    let persist = parsed.is_some();
    let mut settings = parsed.unwrap_or_default();
    if settings.data_dir.is_empty() {
        settings.data_dir = data_dir.to_string_lossy().to_string();
    }
    if persist
        && let Ok(json) = settings.to_stored_json()
            && stored.as_deref() != Some(json.as_str()) {
                let _ = db.set_setting("app", &json);
            }
    settings
}

fn spawn_scheduler(state: Arc<AppState>) {
    spawn_background("scheduler", move || {
        let started = Instant::now();
        let mut last_cleanup = Instant::now();
        let mut last_reconcile = Instant::now();
        let mut last_watch_retry = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(5));
            state.flush_notify();
            let (cleanup_every, reconcile_every) = {
                let s = state.settings.read().unwrap_or_else(|e| e.into_inner());
                (
                    Duration::from_secs(s.cleanup_interval_hours.max(1) as u64 * 3600),
                    Duration::from_secs(s.reconcile_interval_minutes.max(1) as u64 * 60),
                )
            };
            if last_watch_retry.elapsed() >= Duration::from_secs(60) {
                last_watch_retry = Instant::now();
                if let Err(err) = state.refresh_watchers() {
                    log::warn!("watcher refresh failed: {err:#}");
                }
            }
            let settings_trusted = state.db.get_setting("app.bak").ok().flatten().is_none();
            if settings_trusted
                && started.elapsed() >= Duration::from_secs(600)
                && last_cleanup.elapsed() >= cleanup_every
            {
                last_cleanup = Instant::now();
                match retention::run_cleanup(&state, db::now_millis()) {
                    Ok(report) => log::info!(
                        "scheduled cleanup removed {} versions and {} blobs",
                        report.removed_versions,
                        report.removed_blobs
                    ),
                    Err(err) => log::error!("scheduled cleanup failed: {err:#}"),
                }
                state.notify();
            }
            if last_reconcile.elapsed() >= reconcile_every {
                last_reconcile = Instant::now();
                if !state.paused() {
                    for folder in state.db.list_folder_rows().unwrap_or_default() {
                        state.send(Job::Reconcile { folder_id: folder.id });
                    }
                }
            }
        }
    });
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            extract::enable_pdf_helper();
            let data_dir = default_data_dir();
            fs::create_dir_all(&data_dir)?;

            let logger = FileLogger::new(data_dir.join("app.log"));
            let _ = log::set_boxed_logger(Box::new(logger));
            log::set_max_level(LevelFilter::Info);

            let db = Arc::new(Db::open(&data_dir.join("history.db"))?);
            let store = Arc::new(BlobStore::new(data_dir.clone())?);
            let settings = load_settings(&db, &data_dir);
            let start_minimized = settings.start_minimized;
            let paused = settings.paused;

            let (tx, rx) = channel::<Job>();
            let state = AppState::new(db, store, settings, tx);
            *state.handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(app.handle().clone());

            let worker_state = Arc::clone(&state);
            spawn_background("ingest-worker", move || ingest::worker_loop(worker_state, rx));

            app.manage(Arc::clone(&state));

            if let Err(err) = state.refresh_watchers() {
                log::error!("cannot start watchers: {err:#}");
            }
            for folder in state.db.list_folder_rows().unwrap_or_default() {
                state.send(Job::Reconcile { folder_id: folder.id });
            }

            build_tray(app.handle(), paused)?;
            spawn_scheduler(Arc::clone(&state));
            spawn_background("temp-cleanup", || {
                tempfiles::cleanup(Some(Duration::from_secs(24 * 3600)));
            });

            if start_minimized {
                hide_main_window(app.handle());
            } else {
                show_main_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                hide_main_window(window.app_handle());
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_folders,
            commands::add_folder,
            commands::update_folder,
            commands::remove_folder,
            commands::resume_folder,
            commands::list_files,
            commands::get_history,
            commands::get_preview,
            commands::compare,
            commands::restore_version_as,
            commands::restore_version_in_place,
            commands::restore_folder_to_point_in_time,
            commands::suggest_restore_path,
            commands::set_version_pinned,
            commands::set_version_label,
            commands::snapshot_all,
            commands::rescan_all,
            commands::run_cleanup_now,
            commands::preview_cleanup,
            commands::apply_retention_to_all_folders,
            commands::get_stats,
            commands::check_integrity,
            commands::get_settings,
            commands::save_settings,
            commands::set_paused,
            commands::default_excludes,
            commands::reveal_path,
            commands::open_version,
            commands::open_in_diff_tool,
            commands::set_autostart,
        ])
        .build(tauri::generate_context!())
        .expect("cannot build the application")
        .run(|_app, event| match event {
            RunEvent::ExitRequested { .. } => log::info!("application is shutting down"),
            RunEvent::Exit => {
                tempfiles::cleanup(None);
            }
            _ => {}
        });
}
