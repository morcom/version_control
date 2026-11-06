use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use zstd_safe::{CParameter, CCtx, DCtx};

pub const COMPRESSION_LEVEL: i32 = 3;
pub const DELTA_WINDOW_LOG: u32 = 27;

pub const MAX_BLOB_BYTES: usize = 512 * 1024 * 1024;
const INVALID_HASH: &str = "zz-invalid";

pub fn is_valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub struct BlobStore {
    root: PathBuf,
}

fn zstd_error(code: usize) -> anyhow::Error {
    anyhow::anyhow!("zstd error: {}", zstd_safe::get_error_name(code))
}

impl BlobStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("objects"))
            .with_context(|| format!("cannot create object store at {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn check_files(&self, blobs: &[(String, i64)]) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        let mut known = std::collections::HashSet::new();
        for (storage_hash, stored_size) in blobs {
            if !is_valid_hash(storage_hash) {
                problems.push("a blob in the database has an invalid object id".to_string());
                continue;
            }
            let path = self.object_path(storage_hash);
            known.insert(path.clone());
            match fs::metadata(&path) {
                Ok(meta) if meta.len() as i64 == *stored_size => {}
                Ok(_) => problems.push(format!("object {storage_hash} has an unexpected size")),
                Err(_) => problems.push(format!("object {storage_hash} is missing on disk")),
            }
        }
        let orphans = self
            .object_files()?
            .into_iter()
            .filter(|p| !known.contains(p))
            .count();
        if orphans > 0 {
            problems.push(format!("{orphans} object files on disk are not referenced by the database"));
        }
        Ok(problems)
    }

    pub fn object_path(&self, storage_hash: &str) -> PathBuf {
        let safe = if is_valid_hash(storage_hash) { storage_hash } else { INVALID_HASH };
        self.root
            .join("objects")
            .join(&safe[..2])
            .join(format!("{safe}.zst"))
    }

    fn checked_path(&self, storage_hash: &str) -> Result<PathBuf> {
        if !is_valid_hash(storage_hash) {
            anyhow::bail!("invalid object id {:?} in the history database", storage_hash.chars().take(80).collect::<String>());
        }
        Ok(self.object_path(storage_hash))
    }

    pub fn compress_full(data: &[u8]) -> Result<Vec<u8>> {
        let mut cctx = CCtx::create();
        cctx.set_parameter(CParameter::CompressionLevel(COMPRESSION_LEVEL))
            .map_err(zstd_error)?;
        let bound = zstd_safe::compress_bound(data.len());
        let mut out = Vec::with_capacity(bound);
        let written = cctx.compress2(&mut out, data).map_err(zstd_error)?;
        out.truncate(written);
        Ok(out)
    }

    pub fn compress_delta(base: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        let mut cctx: CCtx<'_> = CCtx::create();
        cctx.set_parameter(CParameter::CompressionLevel(COMPRESSION_LEVEL))
            .map_err(zstd_error)?;
        cctx.set_parameter(CParameter::WindowLog(DELTA_WINDOW_LOG))
            .map_err(zstd_error)?;
        cctx.set_parameter(CParameter::EnableLongDistanceMatching(true))
            .map_err(zstd_error)?;
        cctx.ref_prefix(base).map_err(zstd_error)?;
        let bound = zstd_safe::compress_bound(data.len());
        let mut out = Vec::with_capacity(bound);
        let written = cctx.compress2(&mut out, data).map_err(zstd_error)?;
        out.truncate(written);
        Ok(out)
    }

    pub fn decompress_full(data: &[u8], expected_len: usize) -> Result<Vec<u8>> {
        let mut dctx = DCtx::create();
        let capacity = frame_capacity(data, expected_len)?;
        let mut out = Vec::with_capacity(capacity);
        let written = dctx.decompress(&mut out, data).map_err(zstd_error)?;
        out.truncate(written);
        Ok(out)
    }

    pub fn decompress_delta(base: &[u8], data: &[u8], expected_len: usize) -> Result<Vec<u8>> {
        let mut dctx: DCtx<'_> = DCtx::create();
        dctx.ref_prefix(base).map_err(zstd_error)?;
        let capacity = frame_capacity(data, expected_len)?;
        let mut out = Vec::with_capacity(capacity);
        let written = dctx.decompress(&mut out, data).map_err(zstd_error)?;
        out.truncate(written);
        Ok(out)
    }

    pub fn write_blob(&self, storage_hash: &str, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.checked_path(storage_hash)?;
        if path.exists() {
            return Ok(path);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), unique));
        {
            let mut file = fs::File::create(&tmp)
                .with_context(|| format!("cannot create {}", tmp.display()))?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        if let Err(err) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            if !path.exists() {
                return Err(err).with_context(|| format!("cannot store {}", path.display()));
            }
        }
        Ok(path)
    }

    pub fn read_stored(&self, storage_hash: &str) -> Result<Vec<u8>> {
        let path = self.checked_path(storage_hash)?;
        fs::read(&path).with_context(|| format!("cannot read {}", path.display()))
    }

    pub fn remove_stored(&self, storage_hash: &str) {
        match self.checked_path(storage_hash) {
            Ok(path) => {
                let _ = fs::remove_file(path);
            }
            Err(err) => log::warn!("not removing object: {err:#}"),
        }
    }

    pub fn object_files(&self) -> Result<Vec<PathBuf>> {
        let objects = self.root.join("objects");
        if !objects.exists() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(&objects)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            for item in fs::read_dir(entry.path())? {
                let item = item?;
                let path = item.path();
                if path.extension().and_then(|e| e.to_str()) == Some("zst") {
                    files.push(path);
                }
            }
        }
        Ok(files)
    }

    pub fn storage_hash(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }
}

fn frame_capacity(data: &[u8], expected_len: usize) -> Result<usize> {
    let from_frame = zstd_safe::get_frame_content_size(data).ok().flatten();
    match from_frame {
        Some(size) if expected_len > 0 && size != expected_len as u64 => {
            anyhow::bail!("corrupt object: frame size {size} does not match the expected {expected_len} bytes")
        }
        _ if expected_len > 0 => Ok(expected_len),
        Some(size) if size as usize <= MAX_BLOB_BYTES => Ok((size as usize).max(1)),
        Some(size) => anyhow::bail!("corrupt object: frame claims {size} bytes"),
        None => anyhow::bail!("corrupt object: unknown content size"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(size: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(size);
        let mut state: u32 = 12345;
        for _ in 0..size {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            data.push((state >> 16) as u8);
        }
        data
    }

    #[test]
    fn full_blob_roundtrip() {
        let data = sample(200_000);
        let compressed = BlobStore::compress_full(&data).unwrap();
        let restored = BlobStore::decompress_full(&compressed, data.len()).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn compressible_content_shrinks() {
        let data = "the quick brown fox jumps over the lazy dog\n".repeat(5_000);
        let compressed = BlobStore::compress_full(data.as_bytes()).unwrap();
        assert!(compressed.len() < data.len() / 10);
        let restored =
            BlobStore::decompress_full(&compressed, data.len()).unwrap();
        assert_eq!(restored, data.as_bytes());
    }

    #[test]
    fn delta_roundtrip_for_small_change() {
        let base = sample(400_000);
        let mut changed = base.clone();
        changed[200_000] ^= 0xff;
        changed[200_001] ^= 0x0f;
        let delta = BlobStore::compress_delta(&base, &changed).unwrap();
        assert!(delta.len() < changed.len());
        let restored = BlobStore::decompress_delta(&base, &delta, changed.len()).unwrap();
        assert_eq!(restored, changed);
    }

    #[test]
    fn delta_roundtrip_for_inserted_block() {
        let base = sample(300_000);
        let mut changed = base[..150_000].to_vec();
        changed.extend(sample(50_000));
        changed.extend_from_slice(&base[150_000..]);
        let delta = BlobStore::compress_delta(&base, &changed).unwrap();
        let restored = BlobStore::decompress_delta(&base, &delta, changed.len()).unwrap();
        assert_eq!(restored, changed);
    }

    #[test]
    fn delta_roundtrip_for_removed_block() {
        let base = sample(300_000);
        let mut changed = base[..150_000].to_vec();
        changed.extend_from_slice(&base[200_000..]);
        let delta = BlobStore::compress_delta(&base, &changed).unwrap();
        let restored = BlobStore::decompress_delta(&base, &delta, changed.len()).unwrap();
        assert_eq!(restored, changed);
    }

    #[test]
    fn delta_roundtrip_for_repeated_content() {
        let base = "line\n".repeat(50_000).into_bytes();
        let changed = format!("{}{}", "line\n".repeat(50_000), "extra\n");
        let delta = BlobStore::compress_delta(&base, changed.as_bytes()).unwrap();
        let restored =
            BlobStore::decompress_delta(&base, &delta, changed.len()).unwrap();
        assert_eq!(restored, changed.as_bytes());
    }

    #[test]
    fn invalid_object_ids_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        for bad in ["..\\..\\boot", "é1", "", "ABCDEF", &"0".repeat(63)] {
            assert!(store.read_stored(bad).is_err(), "{bad:?} must be rejected");
            store.remove_stored(bad);
            assert!(store.object_path(bad).starts_with(dir.path().join("objects")));
        }
        assert!(is_valid_hash(&"a".repeat(64)));
    }

    #[test]
    fn mismatched_frame_size_is_reported_as_corrupt() {
        let data = b"hello world".repeat(100);
        let compressed = BlobStore::compress_full(&data).unwrap();
        assert!(BlobStore::decompress_full(&compressed, data.len() + 1).is_err());
        assert!(BlobStore::decompress_full(&compressed, 0).is_ok());
    }

    #[test]
    fn empty_content_roundtrips() {
        let compressed = BlobStore::compress_full(b"").unwrap();
        assert_eq!(BlobStore::decompress_full(&compressed, 0).unwrap(), b"");
    }

    #[test]
    fn blobs_are_written_atomically_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf()).unwrap();
        let data = sample(50_000);
        let compressed = BlobStore::compress_full(&data).unwrap();
        let hash = BlobStore::storage_hash(&compressed);
        store.write_blob(&hash, &compressed).unwrap();
        assert!(store.object_path(&hash).exists());
        let read = store.read_stored(&hash).unwrap();
        assert_eq!(read, compressed);
    }
}

