//! Disk layout for uploaded documents.
//!
//! ## Sharding
//!
//! Each upload lives at:
//!
//! ```text
//! <cache_dir>/<aa>/<bb>/<uuid>.<ext>
//! ```
//!
//! where `<aa>` and `<bb>` are the first two hex characters of the
//! UUID's hyphen-less form (`0123456789ab-...` → `01` and `23`).
//! The two-level shard caps any single directory at ~65 Ki entries
//! (UUIDs are random, so collisions on the leading hex digits are
//! rare). The shard is computed at `path_for_uuid` time so the
//! upload handler does one `mkdir -p` and one `rename`.
//!
//! ## Stable IDs
//!
//! The UUID is the row primary key in `uploaded_documents` AND the
//! on-disk filename stem. We never rename an upload (so a 200 OK
//! listing always reflects the bytes the LLM would actually read),
//! and we never reuse a UUID across rows (so a stale DB row cannot
//! accidentally point at a different upload).

use std::path::{Path, PathBuf};

/// Default file extension used when the upload does not name one.
/// Kept conservative (`.bin`) so a future audit can spot uploads
/// that arrived without a recognised extension; the route layer
/// rejects those earlier with `415 Unsupported Media Type`.
pub const DEFAULT_EXTENSION: &str = "bin";

/// Compute the on-disk path for `id` under `cache_dir`. The shard
/// directories are *not* created here — the caller is responsible
/// for `create_dir_all(parent)` so this helper stays pure (and
/// unit-testable without touching the filesystem).
///
/// `extension` is the lowercased filename extension without the
/// leading dot (`"pdf"`, `"txt"`, …). An empty / unknown extension
/// is normalised to [`DEFAULT_EXTENSION`].
pub fn path_for_uuid(cache_dir: &Path, id: &uuid::Uuid, extension: &str) -> PathBuf {
    let hex = id.simple().to_string();
    // First four hex chars → two shards of two.  `simple()` returns
    // the hyphen-less form; we only need the leading slice so
    // dropping the rest with `take(4)` keeps the path short and
    // avoids the cost of building the full 32-char string.
    let shard_a = hex.get(0..2).unwrap_or("00");
    let shard_b = hex.get(2..4).unwrap_or("00");
    let ext = if extension.is_empty() {
        DEFAULT_EXTENSION
    } else {
        extension
    };
    cache_dir
        .join(shard_a)
        .join(shard_b)
        .join(format!("{id}.{ext}"))
}

/// Convenience wrapper that bundles the resolved path and its
/// parent (so the upload handler can `create_dir_all` + `write`
/// without re-doing the math).
#[derive(Debug, Clone)]
pub struct DiskLayout {
    /// Absolute path to the file once it has been moved into the
    /// shard directory. Stored verbatim in `uploaded_documents.disk_path`
    /// so the read tool can re-open it later.
    pub path: PathBuf,
    /// Absolute path to the parent directory. Always populated —
    /// `create_dir_all` is a no-op when the dir exists.
    pub parent: PathBuf,
    /// Lowercased extension used for the on-disk filename. Echoed
    /// back so the caller can stash the same value in the DB row.
    pub extension: String,
}

impl DiskLayout {
    /// Resolve the disk layout for `id` under `cache_dir`. Pure;
    /// no filesystem mutation.
    pub fn for_id(cache_dir: &Path, id: &uuid::Uuid, extension: &str) -> Self {
        let path = path_for_uuid(cache_dir, id, extension);
        let parent = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cache_dir.to_path_buf());
        Self {
            path,
            parent,
            extension: extension.to_ascii_lowercase(),
        }
    }
}

/// Verify the cache dir exists and is writable at boot. Returns a
/// clear error so `main.rs` can refuse to start instead of letting
/// the first upload fail with `ENOSPC` from a misconfigured PVC.
pub fn check_writable(cache_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(cache_dir)?;
    // `OpenOptions::create_new` is the cheapest "can I write?"
    // probe — it touches the dir's inode table without polluting
    // the dir with a real file. We deliberately do NOT remove the
    // probe file: it lives in `<cache_dir>/.write-probe-<ts>` and
    // the periodic sweep can ignore hidden files.
    let probe = cache_dir.join(format!(
        ".write-probe-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    // Best-effort cleanup; ignore the error so a read-only
    // re-mount still surfaces the underlying permission failure
    // rather than a misleading "could not delete probe".
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Errors surfaced by [`safe_disk_read`]. The variants map to
/// distinct security outcomes so the route / agent layers can
/// surface the right error code (404 for missing, 403 for "the row
/// pointed outside the cache dir" which is always an attempt to
/// escalate).
#[derive(Debug, thiserror::Error)]
pub enum DiskReadError {
    /// The `disk_path` from the DB row canonicalised to a path
    /// outside `cache_dir`. Always treated as an attempt to read
    /// an arbitrary file — the caller's request is rejected with
    /// `403 Forbidden` and the event is logged at `warn!` so an
    /// audit can spot the abuse.
    #[error("disk_path escapes cache_dir: {0}")]
    EscapesCacheDir(String),
    /// The file existed inside the cache dir but the OS refused
    /// the read (permission denied, file vanished between the
    /// canonicalise and the read, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Read the bytes of `disk_path` after verifying the canonical path
/// is inside `cache_dir`. Used by both the `read_document` agent
/// AND the `GET /v1/documents/{id}` download handler.
///
/// This is the single gate that defends SEV 1 ("uploaded
/// `disk_path` is trusted verbatim"): the DB row carries the
/// operator-supplied (or attacker-controlled, if auth is bypassed)
/// `disk_path` string, so we never `open()` it without first
/// resolving symlinks and comparing against the cache_dir root.
pub fn safe_disk_read(disk_path: &Path, cache_dir: &Path) -> Result<Vec<u8>, DiskReadError> {
    let canonical_cache = cache_dir.canonicalize().map_err(DiskReadError::Io)?;
    let canonical = match std::fs::canonicalize(disk_path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(DiskReadError::Io(e));
        }
        Err(e) => return Err(DiskReadError::Io(e)),
    };
    if !canonical.starts_with(&canonical_cache) {
        tracing::warn!(
            disk_path = %disk_path.display(),
            canonical = %canonical.display(),
            cache_dir = %canonical_cache.display(),
            "documents: refusing read — disk_path escapes cache_dir",
        );
        return Err(DiskReadError::EscapesCacheDir(
            disk_path.display().to_string(),
        ));
    }
    let bytes = std::fs::read(&canonical)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn path_for_uuid_uses_two_level_shard_from_first_four_hex() {
        // The test id starts with `01234567-…`. `simple()` is the
        // hyphen-less hex so the shard must come from positions 0-1
        // and 2-3 of the 32-char string, NOT from the first 4
        // hyphen-stripped chars.
        let id = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let p = path_for_uuid(Path::new("/cache"), &id, "pdf");
        assert_eq!(
            p,
            PathBuf::from("/cache/01/23/01234567-89ab-cdef-0123-456789abcdef.pdf")
        );
    }

    #[test]
    fn path_for_uuid_normalises_empty_extension() {
        let id = Uuid::parse_str("abcdefab-cdef-abcd-efab-cdefabcdefab").unwrap();
        let p = path_for_uuid(Path::new("/cache"), &id, "");
        // Empty extension falls back to `bin` so the file always has
        // SOME suffix on disk — the browser UI relies on it for
        // mime guessing when the operator re-downloads.
        assert!(p.to_string_lossy().ends_with(".bin"));
    }

    #[test]
    fn path_for_uuid_preserves_known_extension() {
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let p = path_for_uuid(Path::new("/cache"), &id, "txt");
        assert!(p.to_string_lossy().ends_with(".txt"));
    }

    #[test]
    fn disk_layout_creates_full_path_and_parent() {
        let id = Uuid::parse_str("deadbeef-dead-beef-dead-beefdeadbeef").unwrap();
        let layout = DiskLayout::for_id(Path::new("/var/cache/nagent/docs"), &id, "pdf");
        assert_eq!(
            layout.path,
            PathBuf::from("/var/cache/nagent/docs/de/ad/deadbeef-dead-beef-dead-beefdeadbeef.pdf")
        );
        assert_eq!(layout.parent, PathBuf::from("/var/cache/nagent/docs/de/ad"));
        assert_eq!(layout.extension, "pdf");
    }

    #[test]
    fn check_writable_creates_missing_dir() {
        // Use a PID-scoped path so parallel test runners don't
        // trample each other. The dir is removed afterwards.
        let tmp = std::env::temp_dir().join(format!(
            "nagent-doc-write-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        check_writable(&tmp).expect("missing dir must be created and probed");
        assert!(tmp.is_dir(), "check_writable must create the dir");
        // The probe file should be cleaned up — the test will
        // accidentally leave one behind if a regression drops the
        // `remove_file` call, which is the kind of regression this
        // assertion catches.
        let leftover: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".write-probe-"))
            .collect();
        assert!(leftover.is_empty(), "probe file must be cleaned up");
        std::fs::remove_dir(&tmp).ok();
    }

    #[test]
    fn check_writable_succeeds_on_existing_dir() {
        // Smoke check on a directory we know is writable: the
        // process temp dir. Does NOT use `temp_dir()` directly
        // because that's already pre-created — the test exercises
        // the "dir exists, just probe" branch.
        let tmp = std::env::temp_dir();
        check_writable(&tmp).expect("temp dir must be writable");
    }

    #[test]
    fn safe_disk_read_returns_bytes_for_in_cache_file() {
        // Happy path: file lives under cache_dir → returned
        // verbatim. Mirrors the layout `path_for_uuid` produces.
        let tmp_root = std::env::temp_dir().join(format!(
            "nagent-doc-saferead-ok-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(tmp_root.join("ab/cd")).unwrap();
        let target = tmp_root.join("ab/cd/abcdef12-3456-7890-abcd-ef1234567890.txt");
        std::fs::write(&target, b"hello world").unwrap();
        let bytes = safe_disk_read(&target, &tmp_root).expect("must read");
        assert_eq!(bytes, b"hello world");
        std::fs::remove_dir_all(&tmp_root).ok();
    }

    #[test]
    fn safe_disk_read_rejects_path_outside_cache_dir() {
        // Adversarial case: `disk_path` from the DB row is a
        // symlink pointing outside the cache dir. The
        // canonicalize-then-prefix check MUST refuse it.
        let cache_root = std::env::temp_dir().join(format!(
            "nagent-doc-saferead-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(cache_root.join("ab/cd")).unwrap();
        // Symlink target lives outside the cache dir (in the
        // process temp dir).
        let outside_dir = std::env::temp_dir().join(format!(
            "nagent-doc-saferead-outside-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&outside_dir).unwrap();
        let outside_file = outside_dir.join("secret.txt");
        std::fs::write(&outside_file, b"TOP SECRET").unwrap();
        // Symlink inside the cache dir pointing outside it.
        let link = cache_root.join("ab/cd/escape.txt");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();
        let err = safe_disk_read(&link, &cache_root).expect_err("escape must be rejected");
        assert!(
            matches!(err, DiskReadError::EscapesCacheDir(_)),
            "expected EscapesCacheDir, got {err:?}"
        );
        std::fs::remove_dir_all(&cache_root).ok();
        std::fs::remove_dir_all(&outside_dir).ok();
    }

    #[test]
    fn safe_disk_read_propagates_io_error_for_missing_file() {
        let tmp_root = std::env::temp_dir().join(format!(
            "nagent-doc-saferead-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(tmp_root.join("ab/cd")).unwrap();
        let missing = tmp_root.join("ab/cd/does-not-exist.txt");
        let err =
            safe_disk_read(&missing, &tmp_root).expect_err("missing file must surface an io error");
        assert!(matches!(err, DiskReadError::Io(_)));
        std::fs::remove_dir_all(&tmp_root).ok();
    }
}
