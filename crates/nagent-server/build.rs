//! Build script for `nagent-server`.
//!
//! Three responsibilities:
//!
//! 1. Compute a content hash of every file under `src/static/` and
//!    expose it two ways:
//!    - To the Rust crate, via `$OUT_DIR/frontend_version.txt` (read at
//!      runtime with `include_str!`).
//!    - To the browser, via `src/static/version.txt` (re-served at
//!      `/static/version.txt` by `rust-embed`).
//!
//!    The hash lets the chat UI detect that a server rebuild has
//!    happened and that the loaded page is stale.
//!
//! 2. Bake the hash into `src/static/app.js` by replacing the
//!    `APP_JS_PLACEHOLDER` literal. The runtime UI compares this
//!    baked-in value against the server's `/api/version` `frontend`
//!    field so a stale tab (whose loaded bundle carries an OLD hash)
//!    can detect that the server has been rebuilt under it. Without
//!    this step the UI only ever compared two values that come from
//!    the same disk file (`/static/version.txt` and `/api/version`'s
//!    `frontend`), so the banner never fired.
//!
//! 3. Re-emit `cargo:rustc-link-lib` directives for `sonic` and
//!    `pcaudio` whenever the `tts` cargo feature is enabled. This
//!    works around two upstream packaging bugs in `espeak-rs-sys`
//!    0.2.0 (auto-detected but never relayed by its build script):
//!    - espeak-ng's `wavegen.o` references `sonicCreateStream` etc.
//!      (libsonic) but the link directive is dropped.
//!    - espeak-ng's `speech.c` references `audio_object_open` etc.
//!      (libpcaudio) -- same.
//!
//!    All three libs ship as system packages on every distro we care
//!    about. Linking dynamically is preferred for the smaller binary.
//!
//! Without the `tts` feature, only responsibilities 1 + 2 run -- the
//! linker directives are gated behind `#[cfg(feature = "tts")]`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Directory whose contents feed the frontend hash. Resolved relative
/// to `CARGO_MANIFEST_DIR` (the crate root at build time).
const STATIC_DIR: &str = "src/static";

/// Where the hash gets written for the Rust runtime to pick up.
const OUT_FILE: &str = "frontend_version.txt";

/// Same hash, also dropped inside `src/static/` so the browser can
/// fetch it via `/static/version.txt`. Must live next to the other
/// embedded assets so `rust-embed` picks it up at compile time.
const BROWSER_FILE: &str = "version.txt";

/// Frontend entry-point module. The build script substitutes the
/// frontend hash into this file so the loaded bundle can report its
/// own build version without making a separate fetch (which would
/// always return the latest disk value and could never detect drift).
const APP_JS: &str = "app.js";

/// Marker the JS source ships with in the value slot of the
/// `__nagentSelfFrontendVersion` constant. `build.rs` looks up the
/// constant declaration by name and rewrites its string value with
/// the 16-char frontend hash on every build, so the running bundle
/// carries its build version. The placeholder is only meaningful on
/// the very first build -- subsequent builds find the previously
/// baked hash in the same slot and rewrite it again.
const APP_JS_PLACEHOLDER: &str = "__NAGENT_FRONTEND_VERSION_AT_BUILD_TIME__";

fn main() {
    // Re-emit linker directives for the TTS audio backend libs when
    // the `tts` cargo feature is on. See module-level docs for why.
    println!("cargo:rerun-if-env-changed=SONIC_LINK_KIND");
    println!("cargo:rerun-if-env-changed=PCAUDIO_LINK_KIND");
    #[cfg(feature = "tts")]
    {
        // Default to dynamic linking against the system libs.
        // Operators who need static links (rare; usually for embedded
        // deployments) can override with `*_LINK_KIND=static`.
        let sonic_kind = std::env::var("SONIC_LINK_KIND").unwrap_or_else(|_| "dylib".into());
        let pcaudio_kind = std::env::var("PCAUDIO_LINK_KIND").unwrap_or_else(|_| "dylib".into());
        println!("cargo:rustc-link-lib={sonic_kind}=sonic");
        println!("cargo:rustc-link-lib={pcaudio_kind}=pcaudio");
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let static_dir = manifest_dir.join(STATIC_DIR);

    // Rerun this script whenever any file under src/static changes.
    println!("cargo:rerun-if-changed={}", static_dir.display());
    // Rerun if the script itself is updated.
    println!("cargo:rerun-if-changed=build.rs");
    // Rerun when any migration file changes. `sqlx::migrate!`
    // emits `include_str!` for each known migration at compile
    // time, so without this directive adding a new migration
    // would NOT trigger a rebuild of the binaries / tests that
    // embed the migrator static.
    println!("cargo:rerun-if-changed=migrations");

    // Build-time invariant: we need a stable hash even when the static
    // directory is empty (first build, freshly cloned repo). We still
    // want the hash to change across builds if files are added, so we
    // hash the sorted file list -- an empty list hashes to a known value.
    let hash = compute_hash(&static_dir);

    // Emit to $OUT_DIR for include_str! consumption in Rust code.
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let out_path = out_dir.join(OUT_FILE);
    write_version(&out_path, &hash);

    // Emit a sibling copy inside src/static/ so the browser can fetch
    // it. Only do this when we actually have the directory -- running
    // `cargo check` on a freshly cloned repo before the static files
    // exist would otherwise panic.
    if static_dir.is_dir() {
        write_version(&static_dir.join(BROWSER_FILE), &hash);
        // Bake the hash into `app.js` so the UI can detect a stale
        // bundle. We do this AFTER `compute_hash` so the substituted
        // bytes do not feed into this build's hash. On the next
        // build, `compute_hash` reads the substituted file (containing
        // THIS build's hash) and produces a fresh hash, so the hash
        // tracks the latest build regardless of whether any source
        // file actually changed.
        substitute_app_js_version(&static_dir, &hash);
    }

    println!("cargo:warning=nagent-server frontend hash: {hash}");
}

/// SHA-256 of the concatenation of `<relpath>\0<bytes>` over every file
/// under `dir`, sorted by relative path. Returns the first 16 hex chars.
fn compute_hash(dir: &Path) -> String {
    let mut entries: Vec<PathBuf> = Vec::new();
    if dir.is_dir() {
        collect_files(dir, dir, &mut entries);
    }
    entries.sort();

    let mut hasher = Sha256::new();
    for path in &entries {
        let rel = path.strip_prefix(dir).unwrap_or(path);
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0u8]); // NUL separator between path and bytes
        match fs::read(path) {
            Ok(bytes) => hasher.update(&bytes),
            Err(e) => panic!("failed to read {}: {e}", path.display()),
        }
    }
    let digest = hasher.finalize();
    let hex = format!("{:x}", digest);
    // 16 hex chars = 8 bytes -- short enough to be readable, long
    // enough to make accidental collisions astronomically unlikely.
    hex[..16].to_string()
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out);
        } else if path.is_file() {
            // Skip the file we're about to write, otherwise the hash
            // would include its own previous value and drift forever.
            if path == root.join(BROWSER_FILE) {
                continue;
            }
            out.push(path);
        }
    }
}

fn write_version(path: &Path, hash: &str) {
    let mut f = fs::File::create(path)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", path.display()));
    f.write_all(hash.as_bytes())
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

/// Locate the `const __nagentSelfFrontendVersion = "…";` line in
/// `src/static/app.js` and rewrite its string value with `hash`.
///
/// Returns `Some(new_contents)` when the line was found and replaced,
/// `None` when the file does not yet declare the constant (e.g. a
/// freshly cloned tree that has not been updated to ship it). We
/// deliberately match the value slot rather than a placeholder token
/// so the substitution stays idempotent: the same call replaces both
/// the original placeholder (`APP_JS_PLACEHOLDER`) and a previously-
/// baked hash from an earlier build. Without that, the value would
/// freeze at the first build and every subsequent rebuild would
/// compute a different `version.txt` hash than the bundle's baked-in
/// one, falsely alerting the version-drift banner on every cargo
/// invocation.
fn rewrite_app_js_version(src: &str, hash: &str) -> Option<String> {
    const PREFIX: &str = "const __nagentSelfFrontendVersion = \"";
    let start = src.find(PREFIX)?;
    let value_start = start + PREFIX.len();
    let rest = &src[value_start..];
    let value_end = rest.find('"')?;
    let after_value = value_start + value_end;
    // Sanity check the closing token is `;` -- the value slot is
    // exactly the 16 hex chars of the previous build's hash (or the
    // 36-char placeholder on the very first build). Anything else
    // means the source has drifted from the expected shape and we
    // should refuse to rewrite it rather than corrupt the file.
    let closing = rest[value_end + 1..].chars().next();
    if closing != Some(';') {
        return None;
    }
    let mut out = String::with_capacity(src.len());
    out.push_str(&src[..value_start]);
    out.push_str(hash);
    out.push_str(&src[after_value..]);
    Some(out)
}

fn substitute_app_js_version(static_dir: &Path, hash: &str) {
    let path = static_dir.join(APP_JS);
    let Ok(src) = fs::read_to_string(&path) else {
        return;
    };
    let Some(dst) = rewrite_app_js_version(&src, hash) else {
        return;
    };
    fs::write(&path, dst)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

#[cfg(test)]
mod tests {
    //! Build script tests use a temp directory under `OUT_DIR` so they
    //! do not depend on `src/static/` being real. We cover the two
    //! pieces of logic most likely to regress: the hash excludes
    //! `version.txt` (no drift), and the substitution actually
    //! rewrites the placeholder.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tempdir(label: &str) -> PathBuf {
        let base = std::env::var("OUT_DIR").unwrap_or_else(|_| ".".into());
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(base).join(format!("build_rs_test_{label}_{n}"));
        fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn compute_hash_excludes_version_txt() {
        let dir = tempdir("exclude");
        fs::write(dir.join("a.js"), b"alpha").unwrap();
        fs::write(dir.join("b.css"), b"beta").unwrap();
        let h_without = compute_hash(&dir);
        // Drop in a version.txt with a fresh value. The hash must NOT
        // change because compute_hash ignores that file.
        fs::write(dir.join(BROWSER_FILE), b"placeholder-aaaa").unwrap();
        let h_with = compute_hash(&dir);
        assert_eq!(
            h_without, h_with,
            "compute_hash must ignore version.txt to avoid drift"
        );
    }

    #[test]
    fn substitute_rewrites_placeholder() {
        let dir = tempdir("substitute");
        fs::write(
            dir.join(APP_JS),
            b"const __nagentSelfFrontendVersion = \"__NAGENT_FRONTEND_VERSION_AT_BUILD_TIME__\";\n",
        )
        .unwrap();
        substitute_app_js_version(&dir, "abcdef0123456789");
        let after = fs::read_to_string(dir.join(APP_JS)).unwrap();
        assert!(
            after.contains("const __nagentSelfFrontendVersion = \"abcdef0123456789\";"),
            "placeholder must be replaced with the hash, got: {after}"
        );
    }

    #[test]
    fn substitute_rewrites_previous_hash() {
        // After a previous build the file carries a baked-in hash
        // rather than the placeholder. Re-running the script with a
        // fresh hash must still rewrite the value, otherwise the
        // value slot freezes at the first build and every later
        // rebuild ships a stale `app.js` whose baked-in hash
        // disagrees with `version.txt`.
        let dir = tempdir("rewrite");
        fs::write(
            dir.join(APP_JS),
            b"const __nagentSelfFrontendVersion = \"1111aaaa2222bbbb\";\n",
        )
        .unwrap();
        substitute_app_js_version(&dir, "abcdef0123456789");
        let after = fs::read_to_string(dir.join(APP_JS)).unwrap();
        assert!(
            after.contains("const __nagentSelfFrontendVersion = \"abcdef0123456789\";"),
            "previous hash must be replaced, got: {after}"
        );
        assert!(
            !after.contains("1111aaaa2222bbbb"),
            "old hash must not survive, got: {after}"
        );
    }

    #[test]
    fn substitute_is_noop_when_constant_missing() {
        let dir = tempdir("noop");
        fs::write(dir.join(APP_JS), b"const v = 'no constant here';\n").unwrap();
        substitute_app_js_version(&dir, "abcdef0123456789");
        let after = fs::read_to_string(dir.join(APP_JS)).unwrap();
        assert!(
            after.contains("no constant here"),
            "file should be untouched when the constant is missing, got: {after}"
        );
    }
}
