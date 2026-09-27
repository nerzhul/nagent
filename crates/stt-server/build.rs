//! Build script for `stt-server`.
//!
//! Two responsibilities:
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
//! 2. Re-emit `cargo:rustc-link-lib` directives for `sonic` and
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
//! Without the `tts` feature, only responsibility 1 runs -- the
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
    }

    println!("cargo:warning=stt-server frontend hash: {hash}");
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
