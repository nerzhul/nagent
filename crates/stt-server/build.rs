//! Build script for `stt-server`.
//!
//! Re-emits a `cargo:rustc-link-lib` directive for `sonic` whenever
//! the `tts` cargo feature is enabled. This works around an upstream
//! packaging bug in `espeak-rs-sys` 0.2.0: its build script detects
//! the system `libsonic` (or builds a bundled copy via FetchContent)
//! and wires it into espeak-ng's static link via CMake, but never
//! relays the `-lsonic` link directive to Cargo. The result is that
//! `wavegen.o` references like `sonicCreateStream` are unresolved at
//! link time.
//!
//! `libsonic` ships as a system package on every distro we care
//! about (`pacman -S libsonic` on Arch, `apt install libsonic-dev`
//! on Debian/Ubuntu, `dnf install libsonic` on Fedora). Linking
//! dynamically is preferred because (a) it shrinks our binary and
//! (b) it lets the system update libsonic independently. If you
//! really want a static link, override `SONIC_LINK_KIND=static`.
//!
//! Without the `tts` feature this script is a no-op: no
//! `cargo:rustc-link-lib` directives are emitted, so the binary
//! builds without any link dependency on `libsonic`.

fn main() {
    println!("cargo:rerun-if-env-changed=SONIC_LINK_KIND");

    #[cfg(feature = "tts")]
    {
        // Default to dynamic linking against the system `libsonic`.
        // Operators who need a static link (rare; usually for embedded
        // deployments) can set `SONIC_LINK_KIND=static` in their
        // build environment.
        let kind = std::env::var("SONIC_LINK_KIND").unwrap_or_else(|_| "dylib".into());
        println!("cargo:rustc-link-lib={kind}=sonic");
    }
}
