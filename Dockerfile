# syntax=docker/dockerfile:1.7
#
# Multi-stage build for stt-server.
#
# Stage 1 (builder): compile the workspace in release mode. Pulls in libvulkan
#                    headers because the `vulkan` feature links against
#                    libvulkan at runtime (the headers are needed for the
#                    bindgen step inside whisper-rs-sys).
# Stage 2 (runtime): minimal debian-slim with the binary + static assets,
#                    running as a non-root user.
#
# Build:
#   docker build -f Dockerfile -t nagent/stt-server .
#
# Run with the default mock backend (no model needed):
#   docker run --rm -p 8080:8080 -e WHISPER_MODEL_PATH=/dev/null nagent/stt-server
#
# Run with the real backend (mount a model):
#   docker run --rm -p 8080:8080 \
#     -v $PWD/ggml-base.bin:/models/ggml-base.bin:ro \
#     -e WHISPER_MODEL_PATH=/models/ggml-base.bin \
#     nagent/stt-server

# ---- Builder --------------------------------------------------------------
FROM rust:1.81-bookworm AS builder

ENV CARGO_TERM_COLOR=never \
    CARGO_HOME=/usr/local/cargo \
    RUST_BACKTRACE=1

# libvulkan-dev is needed because whisper-rs-sys uses bindgen against the
# Vulkan headers; libssl-dev is for the same reason with OpenSSL-using
# crates we transitively depend on.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libvulkan-dev \
        libssl-dev \
        pkg-config \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependencies separately from sources so a code change does not
# invalidate the dependency build.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release \
        --features stt-server/real-backend,stt-core/whisper-rs-backend \
        -p stt-server

# ---- Runtime --------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libvulkan1 \
        libstdc++6 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 1000 --shell /usr/sbin/nologin stt

WORKDIR /app
COPY --from=builder /build/target/release/stt-server /usr/local/bin/stt-server

ENV BIND_ADDR=0.0.0.0:8080 \
    RUST_LOG=info,stt_server=info,stt_core=info \
    WHISPER_MODEL_PATH=/models/ggml-base.bin

EXPOSE 8080
USER stt:stt

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/stt-server"]