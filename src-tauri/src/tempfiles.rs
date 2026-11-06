use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;

const OPENABLE_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "log", "json", "xml", "yaml", "yml", "ini", "cfg", "conf", "toml", "rtf",
    "pdf", "docx", "xlsx", "pptx", "doc", "xls", "ppt", "odt", "ods", "odp", "png", "jpg", "jpeg", "gif", "bmp",
    "tif", "tiff", "webp", "ico", "mp3", "wav", "flac", "mp4", "mkv", "avi", "mov", "zip", "7z",
];

pub fn dir() -> PathBuf {
    std::env::temp_dir().join("file-history-tracker")
}

pub fn can_open_directly(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| OPENABLE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn remove(path: &Path) -> bool {
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
    }
    fs::remove_file(path).is_ok()
}

fn mark_as_internet_download(path: &Path) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_os_string();
        stream.push(":Zone.Identifier");
        let _ = fs::write(PathBuf::from(stream), "[ZoneTransfer]\r\nZoneId=3\r\n");
    }
    let _ = path;
}

pub fn write_copy(version_id: i64, rel_path: &str, bytes: &[u8]) -> Result<PathBuf> {
    let dir = dir();
    fs::create_dir_all(&dir)?;
    let name = Path::new(rel_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let target = dir.join(format!("v{version_id}-{name}"));
    if target.exists() {
        remove(&target);
    }
    fs::write(&target, bytes)?;
    mark_as_internet_download(&target);
    let mut perms = fs::metadata(&target)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(&target, perms)?;
    Ok(target)
}

pub fn cleanup(older_than: Option<Duration>) -> usize {
    let Ok(entries) = fs::read_dir(dir()) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let old_enough = match older_than {
            None => true,
            Some(age) => entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .map(|d| d >= age)
                .unwrap_or(false),
        };
        if old_enough && remove(&path) {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_passive_types_are_opened_directly() {
        assert!(can_open_directly(Path::new("report.DOCX")));
        assert!(can_open_directly(Path::new("notes.txt")));
        for risky in ["setup.exe", "run.ps1", "help.chm", "x.settingcontent-ms", "macro.docm", "s.js", "noext"] {
            assert!(!can_open_directly(Path::new(risky)), "{risky} must not be opened");
        }
    }

    #[test]
    fn copies_are_read_only_marked_and_replaceable() {
        let first = write_copy(-1, "a/test-copy.txt", b"one").unwrap();
        assert!(fs::metadata(&first).unwrap().permissions().readonly());
        #[cfg(windows)]
        {
            let mut stream = first.as_os_str().to_os_string();
            stream.push(":Zone.Identifier");
            let zone = fs::read_to_string(PathBuf::from(stream)).unwrap();
            assert!(zone.contains("ZoneId=3"));
        }
        let second = write_copy(-1, "a/test-copy.txt", b"two").unwrap();
        assert_eq!(fs::read(&second).unwrap(), b"two");
        assert!(remove(&second));
    }
}
