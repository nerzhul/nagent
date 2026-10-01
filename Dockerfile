# syntax=docker/dockerfile:1.7
#
# Multi-stage build for nagent-server.
#
# Stage 1 (builder): compile the workspace in release mode. The GPU backend
#                    is selected with the `BACKEND` build arg:
#                      - `cpu`     : CPU only (no GPU libs in the image).
#                      - `vulkan`  : Vulkan (default; needs libvulkan-dev).
#                      - `cuda`    : CUDA   (needs cuda-nvcc + matching libs).
#                      - `hipblas` : ROCm/HIP (needs ROCm dev libs).
# Stage 2 (runtime): minimal debian-slim with the binary + static assets,
#                    running as a non-root user.
#
# Build (default = Vulkan, matches the gpu-vulkan docker-compose profile):
#   docker build -f Dockerfile -t nagent/nagent-server .
#
# Build for CPU only (no GPU libs):
#   docker build -f Dockerfile --build-arg BACKEND=cpu -t nagent/nagent-server:cpu .
#
# Run with the default mock backend (no model needed):
#   docker run --rm -p 8080:8080 -e WHISPER_MODEL_PATH=/dev/null nagent/nagent-server
#
# Run with the real backend (mount a model):
#   docker run --rm -p 8080:8080 \
#     -v $PWD/ggml-base.bin:/models/ggml-base.bin:ro \
#     -e WHISPER_MODEL_PATH=/models/ggml-base.bin \
#     nagent/nagent-server

# ---- Builder --------------------------------------------------------------
FROM rust:1.81-bookworm AS builder

ARG BACKEND=vulkan

ENV CARGO_TERM_COLOR=never \
    CARGO_HOME=/usr/local/cargo \
    RUST_BACKTRACE=1

# libvulkan-dev is needed because whisper-rs-sys uses bindgen against the
# Vulkan headers (only when BACKEND=vulkan). The CUDA/HIP toolchains are
# not installed in this base image — supply them via a side-car image or
# an extended base if you pick those backends.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libssl-dev \
        pkg-config \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install Vulkan headers only when BACKEND=vulkan; otherwise skip them to
# keep the build lean and avoid misleading users into thinking the binary
# can use Vulkan when it can't.
RUN if [ "$BACKEND" = "vulkan" ]; then \
        apt-get update && apt-get install -y --no-install-recommends \
            libvulkan-dev \
        && rm -rf /var/lib/apt/lists/*; \
    fi

WORKDIR /build

# Cache dependencies separately from sources so a code change does not
# invalidate the dependency build.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Translate the BACKEND arg into the matching Cargo feature set.
# Note: the `nagent-server/...` prefix is only needed when activating a
# feature on a *transitive dependency* from outside its crate. The
# server's own features (`real-backend`) are addressed by bare name
# because the server is the build target (`-p nagent-server`); cargo
# silently ignores a `package/feature` prefix that matches the
# target, which is why older builds of this image fell back to the
# mock Whisper backend despite the BACKEND arg.
RUN case "$BACKEND" in \
        cpu)     FEATURES="real-backend,stt-core/whisper-rs-backend" ;; \
        vulkan)  FEATURES="real-backend,stt-core/whisper-rs-vulkan" ;; \
        cuda)    FEATURES="real-backend,stt-core/whisper-rs-cuda" ;; \
        hipblas) FEATURES="real-backend,stt-core/whisper-rs-hipblas" ;; \
        *) echo "unknown BACKEND=$BACKEND (expected cpu|vulkan|cuda|hipblas)" && exit 1 ;; \
    esac && \
    cargo build --release --features "$FEATURES" -p nagent-server

# ---- Runtime --------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

ARG BACKEND=vulkan

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libstdc++6 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 1000 --shell /usr/sbin/nologin nagent

# Only install the Vulkan loader when the binary was built against Vulkan.
# CUDA / HIP loaders come from the host driver and don't need anything here.
RUN if [ "$BACKEND" = "vulkan" ]; then \
        apt-get update && apt-get install -y --no-install-recommends \
            libvulkan1 \
        && rm -rf /var/lib/apt/lists/*; \
    fi

WORKDIR /app
COPY --from=builder /build/target/release/nagent-server /usr/local/bin/nagent-server

ENV BIND_ADDR=0.0.0.0:8080 \
    RUST_LOG=info,nagent_server=info,stt_core=info \
    WHISPER_MODEL_PATH=/models/ggml-base.bin

EXPOSE 8080
USER nagent:nagent

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/nagent-server"]
