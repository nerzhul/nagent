//! Build script for `stt-server`.
//!
//! Re-emits `cargo:rustc-link-lib` directives for `sonic` and
//! `pcaudio` whenever the `tts` cargo feature is enabled. This
//! works around two upstream packaging bugs in `espeak-rs-sys`
//! 0.2.0:
//!
//! 1. Its build script detects the system `libsonic` (or builds a
//!    bundled copy via FetchContent) and wires it into espeak-ng's
//!    static link via CMake, but never relays the `-lsonic` link
//!    directive to Cargo. `wavegen.o` ends up referencing
//!    `sonicCreateStream` etc. with no link target.
//!
//! 2. espeak-ng defaults to `USE_LIBPCAUDIO=ON` (because
//!    `find_library(PCAUDIO_LIB pcaudio)` succeeds on most systems),
//!    which compiles `speech.c` calls to `audio_object_open`,
//!    `audio_object_write`, `create_audio_device_object`, etc. Those
//!    symbols live in `libpcaudio.so`, which the build script also
//!    forgets to link. The result is `undefined symbol:
//!    audio_object_open` at link time.
//!
//! All three libs (`libsonic`, `libespeak-ng`, `libpcaudio`) ship as
//! system packages on every distro we care about (`pacman -S
//! libsonic libpcaudio` on Arch, `apt install libsonic-dev
//! libpcaudio-dev` on Debian/Ubuntu, `dnf install libsonic
//! libpcaudio` on Fedora). Linking dynamically is preferred because
//! (a) it shrinks our binary and (b) it lets the system update them
//! independently.
//!
//! Without the `tts` feature this script is a no-op: no
//! `cargo:rustc-link-lib` directives are emitted, so the binary
//! builds without any link dependency on `libsonic` or
//! `libpcaudio`.

fn main() {
    println!("cargo:rerun-if-env-changed=SONIC_LINK_KIND");
    println!("cargo:rerun-if-env-changed=PCAUDIO_LINK_KIND");

    #[cfg(feature = "tts")]
    {
        // Default to dynamic linking against the system libs.
        // Operators who need static links (rare; usually for
        // embedded deployments) can override with `*_LINK_KIND=static`.
        let sonic_kind = std::env::var("SONIC_LINK_KIND").unwrap_or_else(|_| "dylib".into());
        let pcaudio_kind = std::env::var("PCAUDIO_LINK_KIND").unwrap_or_else(|_| "dylib".into());
        println!("cargo:rustc-link-lib={sonic_kind}=sonic");
        println!("cargo:rustc-link-lib={pcaudio_kind}=pcaudio");
    }
}
