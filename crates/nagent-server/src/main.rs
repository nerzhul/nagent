//! Server entry point. Owns CLI dispatch, tracing init, config
//! load, and `axum::serve`. All boot wiring (auth bootstrap,
//! OIDC/passkey sub-states, whisper backend, worker pool, LLM
//! client, agent registry, TTS engine, documents store, per-IP
//! rate limiters) lives in [`app::build_app`].

use std::net::SocketAddr;

use nagent_server::app::build_app;
use nagent_server::cli::{auth as auth_cli, documents as documents_cli, migrate as migrate_cli};
use nagent_server::http::build_router;
use nagent_server::{CliArgs, Config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<std::process::ExitCode> {
    // `stt-server auth …` subcommand dispatch happens FIRST so the
    // CLI subcommand arguments (`create-admin`, `--email`, etc.)
    // do not get rejected by the strict `CliArgs::parse` loop below.
    // The dispatch walks the full argv (skipping the binary name)
    // so `--config FOO auth list-users` routes correctly even when
    // the operator puts the global flag before the subcommand.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(idx) = argv.iter().position(|a| a == "auth") {
        // Concatenate leading global flags (`--config FOO`) with
        // the trailing subcommand args (`list-users`). The auth CLI
        // re-parses `--config` from the combined vector and routes
        // the rest to the subcommand parser. This handles both
        // `stt-server auth …` and `stt-server --config FOO auth …`.
        let mut argv = argv;
        let trailing = argv.split_off(idx + 1);
        let mut combined = argv;
        combined.extend(trailing);
        return auth_cli::run_auth_cli(combined).await;
    }
    if let Some(idx) = argv.iter().position(|a| a == "migrate") {
        // Same dispatcher trick as the `auth` branch above. Order
        // matters: `auth` stays first so existing behaviour is
        // unchanged, then `migrate`, then fallthrough to server
        // boot.
        let mut argv = argv;
        let trailing = argv.split_off(idx + 1);
        let mut combined = argv;
        combined.extend(trailing);
        return migrate_cli::run_migrate_cli(combined).await;
    }
    if let Some(idx) = argv.iter().position(|a| a == "documents") {
        // Same dispatcher trick as the `auth` / `migrate` branches
        // above. Sits after both so `--config FOO documents purge
        // …` routes correctly.
        let mut argv = argv;
        let trailing = argv.split_off(idx + 1);
        let mut combined = argv;
        combined.extend(trailing);
        return documents_cli::run_documents_cli(combined).await;
    }

    init_tracing();

    // CLI flags must be parsed before tracing emits its first
    // `info!` so a malformed `--config` exits cleanly without
    // producing a half-initialised log line.
    let cli = CliArgs::parse();
    let cfg = Config::load(&cli).map_err(|e| anyhow::anyhow!("{e}"))?;
    tracing::info!(addr = %cfg.bind_addr, model = ?cfg.whisper_model_path, "starting nagent stt-server");

    let state = build_app(&cfg).await?;
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(cfg.bind_addr).await?;
    tracing::info!("listening on http://{}", cfg.bind_addr);
    // Use the `with_connect_info` variant so the auth login handler
    // can read the peer IP via `ConnectInfo<SocketAddr>` (used for
    // the per-(email, ip) login rate-limit + the audit row). The
    // vanilla `into_make_service` would omit it.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(std::process::ExitCode::SUCCESS)
}

fn init_tracing() {
    // Default filter demotes the ONNX Runtime allocator chatter
    // (`ort::logging` emits BFC-arena allocation lines at `info` on
    // every model load — see the
    // "Allocated memory at 0x…", "Extending BFCArena", "Extended
    // allocation by … bytes" lines) down to `debug` so the runtime
    // log stays usable. Operators who want to see the allocator
    // accounting can opt back in with `RUST_LOG=ort=debug`. The
    // `whisper_rs::*` target is treated the same way for symmetry:
    // whisper-rs emits a per-chunk "mel" trace at info that drowns
    // out everything else during a long session.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,nagent_server=debug,stt_core=debug,ort=debug,ort::logging=debug,whisper_rs=debug",
        )
    });
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
