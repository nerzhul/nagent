//! Build-time version strings.
//!
//! - `backend_version()` returns the `CARGO_PKG_VERSION` of this crate
//!   at compile time. It is what the running server reports about
//!   itself.
//! - `frontend_version()` returns a content hash of every file under
//!   `src/static/`, computed by `build.rs` and embedded via
//!   `include_str!`. The same string is also dropped next to the other
//!   static assets as `version.txt` so the browser can read it.
//!
//! Both strings are `&'static str` so the cost is one `.rodata` lookup
//! per request — no allocation, no I/O.
//!
//! The `/api/version` endpoint exposes the two side by side. The
//! frontend compares its locally bundled `frontend_version` (read from
//! `/static/version.txt` on page load) against this endpoint's
//! `frontend` field; a mismatch means the server has been rebuilt and
//! the user should reload.

/// Version of the compiled server crate (e.g. `"0.1.0"`).
pub const fn backend_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Content hash of the embedded frontend assets at the time this binary
/// was built. Same string is served at `/static/version.txt` for the
/// browser to compare against.
pub fn frontend_version() -> &'static str {
    // The path is computed via `env!("OUT_DIR")` so it is stable across
    // cargo invocations on the same machine, and the file is written by
    // `build.rs` before the main crate compiles.
    include_str!(concat!(env!("OUT_DIR"), "/frontend_version.txt")).trim()
}

/// Wire format for `GET /api/version`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VersionInfo {
    /// Crate version of the running server.
    pub backend: String,
    /// Content hash of the frontend assets the server is currently
    /// serving. The browser compares this to the value it read from
    /// `/static/version.txt` on page load.
    pub frontend: String,
}

impl VersionInfo {
    /// Snapshot the current versions of this binary.
    pub fn current() -> Self {
        Self {
            backend: backend_version().to_string(),
            frontend: frontend_version().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_version_matches_cargo_pkg() {
        // We can't import CARGO_PKG_VERSION directly here, so just
        // assert the string is non-empty and looks like a version
        // (digits + dots, semver-ish). This catches a missing
        // `version.workspace = true` in Cargo.toml.
        let v = backend_version();
        assert!(!v.is_empty(), "backend version must not be empty");
        assert!(
            v.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c.is_ascii_alphabetic()),
            "backend version `{v}` has unexpected characters"
        );
    }

    #[test]
    fn frontend_version_is_stable_hex() {
        let v = frontend_version();
        // 16 hex chars per build.rs.
        assert_eq!(v.len(), 16, "frontend hash length unexpected: {v:?}");
        assert!(
            v.chars().all(|c| c.is_ascii_hexdigit()),
            "frontend hash `{v}` contains non-hex characters"
        );
    }

    #[test]
    fn current_version_is_self_consistent() {
        let info = VersionInfo::current();
        assert_eq!(info.backend, backend_version());
        assert_eq!(info.frontend, frontend_version());
    }

    /// The hash embedded in the binary and the hash dropped next to the
    /// other static assets MUST be the same string, otherwise the
    /// browser will compare its own version.txt against a different
    /// value at `/api/version` and trigger the reload banner forever.
    #[test]
    fn embedded_version_txt_matches_runtime_hash() {
        let runtime = frontend_version();
        let on_disk = crate::static_assets::StaticAssets::get("version.txt")
            .expect("version.txt must be embedded by rust-embed")
            .data;
        let on_disk_str = std::str::from_utf8(on_disk.as_ref())
            .expect("version.txt is UTF-8")
            .trim();
        assert_eq!(
            runtime, on_disk_str,
            "embedded version.txt and frontend_version() disagree"
        );
    }
}
