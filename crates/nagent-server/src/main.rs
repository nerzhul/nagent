//! Server entry point. Owns CLI dispatch, tracing init, config
//! load, and `axum::serve`. All boot wiring (auth bootstrap,
//! OIDC/passkey sub-states, whisper backend, worker pool, LLM
//! client, agent registry, TTS engine, documents store, per-IP
//! rate limiters) lives in [`app::build_app`].

use std::net::SocketAddr;

use clap::Parser;
use nagent_server::cli::{self, Cli};
use nagent_server::http::build_router;
use nagent_server::{app::build_app, Config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<std::process::ExitCode> {
    // `Cli::parse()` (clap derive) owns subcommand detection and the
    // global `--config PATH` knob. When a subcommand is present the
    // binary exits without booting the HTTP server; otherwise it
    // falls through to the server boot path below.
    let cli = Cli::parse();
    match cli.command {
        Some(cli::Command::Auth(c)) => return cli::auth::run(c, cli.config.as_deref()).await,
        Some(cli::Command::Migrate(c)) => return cli::migrate::run(c, cli.config.as_deref()).await,
        Some(cli::Command::Documents(c)) => {
            return cli::documents::run(c, cli.config.as_deref()).await;
        }
        None => {}
    }

    init_tracing();

    // CLI flags must be parsed before tracing emits its first
    // `info!` so a malformed `--config` exits cleanly without
    // producing a half-initialised log line.
    let cfg = Config::load(cli.config.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
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
