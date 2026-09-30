//! `documents` — `[documents]` section: discussion-mode uploads + the
//! `read_document` LLM tool.

use std::path::PathBuf;

use crate::config::file::TomlDocumentsConfig;
use crate::config::{env_opt, resolve_opt_string, resolve_primitive, ConfigError};

/// Configuration for the optional document-upload subsystem.
///
/// `enabled` is the master switch. When `false`, the `/v1/documents*`
/// routes are not mounted and the `read_document` agent is not
/// registered. The config is always parsed (no cargo feature gate on
/// the env / TOML plumbing) so a build without the `documents` cargo
/// feature still surfaces configuration mistakes at boot time — the
/// `documents` feature only controls the heavy deps (`pdf-extract` +
/// `mime_guess`).
#[derive(Debug, Clone)]
pub struct DocumentsConfig {
    /// Master switch for the `/v1/documents*` routes + the
    /// `read_document` agent. Defaults to `false` so first-time users
    /// do not accidentally expose the cache dir.
    pub enabled: bool,
    /// Directory where uploaded files are staged on disk. Defaults to
    /// `/var/cache/nagent/docs` (the kustomize base mounts a PVC
    /// there). The server refuses to start when the dir is not
    /// writable at boot — see `documents::store::check_writable`.
    pub cache_dir: PathBuf,
    /// Maximum upload size, in bytes. Hard cap on the multipart body.
    pub max_file_size_bytes: usize,
    /// Maximum number of characters the server extracts from a
    /// document at upload time. Past the cap the extractor
    /// truncates and adds a `[… truncated …]` marker; the file is
    /// still stored so the LLM can request the relevant page range
    /// later.
    pub max_extracted_chars: usize,
    /// Hard cap on documents per chat session. The 51st upload is
    /// rejected with `429 Too Many Documents`.
    pub max_docs_per_session: u32,
    /// PDF parse timeout in seconds. `pdf-extract` is synchronous so
    /// the cap is enforced by wrapping the call in
    /// `tokio::task::spawn_blocking` + a timeout.
    pub pdf_extract_timeout_secs: u64,
    /// Background sweep interval, in hours. A dedicated `tokio::spawn`
    /// task in `main.rs` calls `purge_older_than` every
    /// `purge_interval_hours`. Set to `0` to disable the background
    /// task (operators who only want the CLI sweep keep working).
    pub purge_interval_hours: u64,
    /// Default document TTL in days. Rows older than `created_at +
    /// default_ttl_days` are purged (file + DB row). Operators can
    /// override per-row with `expires_at`.
    pub default_ttl_days: u32,
}

impl Default for DocumentsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cache_dir: PathBuf::from("/var/cache/nagent/docs"),
            max_file_size_bytes: 20 * 1024 * 1024,
            max_extracted_chars: 100_000,
            max_docs_per_session: 50,
            pdf_extract_timeout_secs: 30,
            purge_interval_hours: 24,
            default_ttl_days: 30,
        }
    }
}

impl DocumentsConfig {
    pub fn from_env_with_toml(toml: Option<&TomlDocumentsConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();
        let cache_dir = resolve_opt_string(
            env_opt("DOCS_CACHE_DIR").as_deref(),
            toml.cache_dir.as_deref(),
        )
        .map(PathBuf::from)
        .unwrap_or(defaults.cache_dir.clone());
        Ok(Self {
            enabled: resolve_primitive(
                env_opt("DOCS_ENABLED").as_deref(),
                toml.enabled,
                defaults.enabled,
                "DOCS_ENABLED",
            )?,
            cache_dir,
            max_file_size_bytes: resolve_primitive(
                env_opt("DOCS_MAX_FILE_BYTES").as_deref(),
                toml.max_file_size_bytes,
                defaults.max_file_size_bytes,
                "DOCS_MAX_FILE_BYTES",
            )?,
            max_extracted_chars: resolve_primitive(
                env_opt("DOCS_MAX_CHARS").as_deref(),
                toml.max_extracted_chars,
                defaults.max_extracted_chars,
                "DOCS_MAX_CHARS",
            )?,
            max_docs_per_session: resolve_primitive(
                env_opt("DOCS_MAX_PER_SESSION").as_deref(),
                toml.max_docs_per_session,
                defaults.max_docs_per_session,
                "DOCS_MAX_PER_SESSION",
            )?,
            pdf_extract_timeout_secs: resolve_primitive(
                env_opt("DOCS_PDF_TIMEOUT_SECS").as_deref(),
                toml.pdf_extract_timeout_secs,
                defaults.pdf_extract_timeout_secs,
                "DOCS_PDF_TIMEOUT_SECS",
            )?,
            purge_interval_hours: resolve_primitive(
                env_opt("DOCS_PURGE_INTERVAL_H").as_deref(),
                toml.purge_interval_hours,
                defaults.purge_interval_hours,
                "DOCS_PURGE_INTERVAL_H",
            )?,
            default_ttl_days: resolve_primitive(
                env_opt("DOCS_TTL_DAYS").as_deref(),
                toml.default_ttl_days,
                defaults.default_ttl_days,
                "DOCS_TTL_DAYS",
            )?,
        })
    }
}
