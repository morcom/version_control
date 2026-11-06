use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

#[derive(Debug, Clone)]
pub enum Job {
    Touch { folder_id: i64, path: PathBuf },
    Removed { folder_id: i64, path: PathBuf },
    Renamed {
        folder_id: i64,
        from: PathBuf,
        to: PathBuf,
    },
    Reconcile { folder_id: i64 },
    Snapshot { folder_id: i64 },
}

const RENAME_PAIR_WINDOW: Duration = Duration::from_millis(500);

struct RenamePairer {
    pending_from: Option<(PathBuf, Option<usize>, Instant)>,
}

impl RenamePairer {
    fn flush(&mut self, folder_id: i64, tx: &Sender<Job>) {
        if let Some((path, _, _)) = self.pending_from.take() {
            let _ = tx.send(Job::Removed { folder_id, path });
        }
    }

    fn take_match(&mut self, tracker: Option<usize>) -> Option<PathBuf> {
        let (path, from_tracker, at) = self.pending_from.take()?;
        let tracker_ok = match (from_tracker, tracker) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        if tracker_ok && at.elapsed() <= RENAME_PAIR_WINDOW {
            Some(path)
        } else {
            self.pending_from = Some((path, from_tracker, at));
            None
        }
    }
}

fn handle_event(
    pairer: &mut RenamePairer,
    folder_id: i64,
    event: notify::Event,
    tx: &Sender<Job>,
) {
    if event.need_rescan() {
        pairer.flush(folder_id, tx);
        let _ = tx.send(Job::Reconcile { folder_id });
        return;
    }
    let tracker = event.tracker();
    match event.kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if event.paths.len() >= 2 => {
            pairer.flush(folder_id, tx);
            let _ = tx.send(Job::Renamed {
                folder_id,
                from: event.paths[0].clone(),
                to: event.paths[1].clone(),
            });
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            pairer.flush(folder_id, tx);
            if let Some(path) = event.paths.into_iter().next() {
                pairer.pending_from = Some((path, tracker, Instant::now()));
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            let Some(to) = event.paths.into_iter().next() else {
                return;
            };
            match pairer.take_match(tracker) {
                Some(from) => {
                    let _ = tx.send(Job::Renamed { folder_id, from, to });
                }
                None => {
                    pairer.flush(folder_id, tx);
                    let _ = tx.send(Job::Touch { folder_id, path: to });
                }
            }
        }
        EventKind::Create(_) => {
            pairer.flush(folder_id, tx);
            for path in event.paths {
                let _ = tx.send(Job::Touch { folder_id, path });
            }
        }
        EventKind::Modify(_) => {
            pairer.flush(folder_id, tx);
            for path in event.paths.into_iter().filter(|p| !p.is_dir()) {
                let _ = tx.send(Job::Touch { folder_id, path });
            }
        }
        EventKind::Remove(_) => {
            pairer.flush(folder_id, tx);
            for path in event.paths {
                let _ = tx.send(Job::Removed { folder_id, path });
            }
        }
        _ => {}
    }
}

impl RenamePairer {
    fn flush_expired(&mut self, folder_id: i64, tx: &Sender<Job>) {
        if self
            .pending_from
            .as_ref()
            .is_some_and(|(_, _, at)| at.elapsed() > RENAME_PAIR_WINDOW)
        {
            self.flush(folder_id, tx);
        }
    }
}

type Shared = Arc<(Mutex<RenamePairer>, Condvar)>;

fn spawn_expiry_timer(shared: Weak<(Mutex<RenamePairer>, Condvar)>, folder_id: i64, tx: Sender<Job>) {
    let spawned = std::thread::Builder::new()
        .name(format!("rename-timer-{folder_id}"))
        .spawn(move || loop {
            let Some(shared) = shared.upgrade() else {
                break;
            };
            let (lock, wake) = &*shared;
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let wait = match guard.pending_from.as_ref() {
                Some((_, _, at)) => RENAME_PAIR_WINDOW.saturating_sub(at.elapsed()) + Duration::from_millis(20),
                None => Duration::from_secs(5),
            };
            let (mut guard, _) = wake.wait_timeout(guard, wait).unwrap_or_else(|e| e.into_inner());
            guard.flush_expired(folder_id, &tx);
        });
    if let Err(err) = spawned {
        log::warn!("cannot start rename timer for folder {folder_id}: {err}");
    }
}

pub fn spawn_watcher(folder_id: i64, root: PathBuf, tx: Sender<Job>) -> Result<RecommendedWatcher> {
    let shared: Shared = Arc::new((Mutex::new(RenamePairer { pending_from: None }), Condvar::new()));
    spawn_expiry_timer(Arc::downgrade(&shared), folder_id, tx.clone());
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let (lock, wake) = &*shared;
        let mut pairer = lock.lock().unwrap_or_else(|e| e.into_inner());
        let had_pending = pairer.pending_from.is_some();
        match res {
            Ok(event) => handle_event(&mut pairer, folder_id, event, &tx),
            Err(err) => {
                pairer.flush(folder_id, &tx);
                log::warn!("watcher error for folder {folder_id}: {err}");
                let _ = tx.send(Job::Reconcile { folder_id });
            }
        }
        if !had_pending && pairer.pending_from.is_some() {
            wake.notify_one();
        }
    })
    .context("cannot create filesystem watcher")?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .with_context(|| format!("cannot watch {}", root.display()))?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, RemoveKind};
    use std::sync::mpsc::channel;

    fn event(kind: EventKind, paths: &[&str]) -> notify::Event {
        let mut e = notify::Event::new(kind);
        for p in paths {
            e = e.add_path(PathBuf::from(p));
        }
        e
    }

    #[test]
    fn windows_style_rename_without_tracker_is_paired() {
        let (tx, rx) = channel();
        let mut pairer = RenamePairer { pending_from: None };
        handle_event(&mut pairer, 1, event(EventKind::Modify(ModifyKind::Name(RenameMode::From)), &["a.txt"]), &tx);
        handle_event(&mut pairer, 1, event(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &["b.txt"]), &tx);
        match rx.try_recv().unwrap() {
            Job::Renamed { from, to, .. } => {
                assert_eq!(from, PathBuf::from("a.txt"));
                assert_eq!(to, PathBuf::from("b.txt"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn unpaired_from_becomes_removed() {
        let (tx, rx) = channel();
        let mut pairer = RenamePairer { pending_from: None };
        handle_event(&mut pairer, 1, event(EventKind::Modify(ModifyKind::Name(RenameMode::From)), &["a.txt"]), &tx);
        handle_event(&mut pairer, 1, event(EventKind::Create(CreateKind::File), &["c.txt"]), &tx);
        assert!(matches!(rx.try_recv().unwrap(), Job::Removed { .. }));
        assert!(matches!(rx.try_recv().unwrap(), Job::Touch { .. }));
    }

    #[test]
    fn unpaired_from_is_flushed_by_the_timer() {
        let (tx, rx) = channel();
        let shared: Shared = Arc::new((Mutex::new(RenamePairer { pending_from: None }), Condvar::new()));
        spawn_expiry_timer(Arc::downgrade(&shared), 1, tx.clone());
        {
            let mut pairer = shared.0.lock().unwrap();
            handle_event(
                &mut pairer,
                1,
                event(EventKind::Modify(ModifyKind::Name(RenameMode::From)), &["moved-away.txt"]),
                &tx,
            );
            shared.1.notify_one();
        }
        let started = std::time::Instant::now();
        match rx.recv_timeout(Duration::from_secs(3)).unwrap() {
            Job::Removed { path, .. } => assert_eq!(path, PathBuf::from("moved-away.txt")),
            other => panic!("unexpected {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(2), "flushed promptly, not after the idle wait");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn directory_modify_events_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = channel();
        let mut pairer = RenamePairer { pending_from: None };
        let dir_path = dir.path().to_string_lossy().to_string();
        handle_event(&mut pairer, 1, event(EventKind::Modify(ModifyKind::Any), &[&dir_path]), &tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn plain_remove_is_forwarded() {
        let (tx, rx) = channel();
        let mut pairer = RenamePairer { pending_from: None };
        handle_event(&mut pairer, 1, event(EventKind::Remove(RemoveKind::File), &["x.txt"]), &tx);
        assert!(matches!(rx.try_recv().unwrap(), Job::Removed { .. }));
    }
}
