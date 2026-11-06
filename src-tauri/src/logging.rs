use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use log::{Level, Metadata, Record};

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub struct FileLogger {
    file: Mutex<Option<File>>,
    path: PathBuf,
    writes: AtomicU64,
}

fn open(path: &PathBuf) -> Option<File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

impl FileLogger {
    pub fn new(log_path: PathBuf) -> Self {
        let logger = Self {
            file: Mutex::new(open(&log_path)),
            path: log_path,
            writes: AtomicU64::new(0),
        };
        logger.rotate_if_needed();
        logger
    }

    pub fn rotate_if_needed(&self) {
        let too_big = std::fs::metadata(&self.path)
            .map(|m| m.len() >= MAX_LOG_BYTES)
            .unwrap_or(false);
        if !too_big {
            return;
        }
        if let Ok(mut guard) = self.file.lock() {
            *guard = None;
            let backup = self.path.with_extension("log.1");
            let _ = std::fs::remove_file(&backup);
            let _ = std::fs::rename(&self.path, &backup);
            *guard = open(&self.path);
        }
    }
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if self.writes.fetch_add(1, Ordering::Relaxed) % 500 == 499 {
            self.rotate_if_needed();
        }
        let Ok(mut guard) = self.file.lock() else {
            return;
        };
        if let Some(file) = guard.as_mut() {
            let _ = writeln!(
                file,
                "{} [{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.args()
            );
            let _ = file.flush();
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut() {
                let _ = file.flush();
            }
    }
}
