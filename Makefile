# nagent — Makefile
# Convenience targets for the Rust workspace + packaging.
# All targets are phony.

SHELL := /usr/bin/env bash
BIN   := stt-server
IMAGE := nagent/stt-server:dev

.DEFAULT_GOAL := help

.PHONY: help
help: ## Show this help.
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# ---- Rust workspace ---------------------------------------------------------

.PHONY: build
build: ## cargo build --workspace --release
	cargo build --workspace --release

.PHONY: debug
debug: ## cargo build --workspace
	cargo build --workspace

.PHONY: run
run: ## Run the server locally with the real Whisper backend on CPU (requires WHISPER_MODEL_PATH).
	cargo run -p stt-server --release \
	    --features stt-server/real-backend,stt-core/whisper-rs-backend

.PHONY: run-vulkan
run-vulkan: ## Run with the Vulkan GPU backend (requires libvulkan-dev at build time).
	cargo run -p stt-server --release \
	    --features stt-server/real-backend,stt-core/whisper-rs-vulkan

.PHONY: run-hipblas
run-hipblas: ## Run with the HIP/ROCm GPU backend (requires ROCm toolchain at build time).
	cargo run -p stt-server --release \
	    --features stt-server/real-backend,stt-core/whisper-rs-hipblas

.PHONY: run-cuda
run-cuda: ## Run with the CUDA GPU backend (requires CUDA toolkit at build time).
	cargo run -p stt-server --release \
	    --features stt-server/real-backend,stt-core/whisper-rs-cuda

.PHONY: run-mock
run-mock: ## Run the server locally with the in-process mock backend (no model needed).
	cargo run -p stt-server --release

.PHONY: run-llm
run-llm: ## Run with the real Whisper backend on CPU and the LLM proxy enabled (LLM_ENABLED=true).
	cargo run -p stt-server --release \
	    --features stt-server/real-backend,stt-core/whisper-rs-backend

.PHONY: smoke-llm
smoke-llm: ## Smoke-test the LLM proxy: POST a single prompt and check the response stream contains `data:`.
	@curl -sS -N -X POST "$${NAGENT_URL:-http://localhost:8080}/v1/chat/completions" \
	    -H "Content-Type: application/json" \
	    -d '{"messages":[{"role":"user","content":"hello"}],"stream":true}' \
	    | grep -q '^data:' && echo "llm proxy: OK (streamed `data:` frames received)"

.PHONY: fmt
fmt: ## cargo fmt --all
	cargo fmt --all

.PHONY: fmt-check
fmt-check: ## cargo fmt --all -- --check
	cargo fmt --all -- --check

.PHONY: clippy
clippy: ## cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy --workspace --all-targets -- -D warnings

.PHONY: test
test: ## cargo test --workspace
	cargo test --workspace --all-features

.PHONY: clean
clean: ## cargo clean
	cargo clean

# ---- Docker -----------------------------------------------------------------

.PHONY: docker-build
docker-build: ## Build the Docker image with the CPU backend (override BACKEND=).
	docker build -f Dockerfile --build-arg BACKEND=cpu -t $(IMAGE) .

.PHONY: docker-build-vulkan
docker-build-vulkan: ## Build the Docker image with the Vulkan backend.
	docker build -f Dockerfile --build-arg BACKEND=vulkan -t $(IMAGE) .

.PHONY: docker-build-cuda
docker-build-cuda: ## Build the Docker image with the CUDA backend.
	docker build -f Dockerfile --build-arg BACKEND=cuda -t $(IMAGE) .

.PHONY: docker-build-hipblas
docker-build-hipblas: ## Build the Docker image with the HIP/ROCm backend.
	docker build -f Dockerfile --build-arg BACKEND=hipblas -t $(IMAGE) .

.PHONY: docker-run-cpu
docker-run-cpu: ## Run on CPU with a model bind-mounted.
	docker run --rm -p 8080:8080 \
	  -v $$(realpath $${WHISPER_MODEL_PATH:-./ggml-base.bin}):/models/ggml-base.bin:ro \
	  -e WHISPER_MODEL_PATH=/models/ggml-base.bin \
	  $(IMAGE)

.PHONY: docker-run-gpu-vulkan
docker-run-gpu-vulkan: ## Run with Vulkan GPU passthrough.
	docker run --rm -p 8080:8080 --device /dev/dri:/dev/dri \
	  -v $$(realpath $${WHISPER_MODEL_PATH:-./ggml-base.bin}):/models/ggml-base.bin:ro \
	  -e WHISPER_MODEL_PATH=/models/ggml-base.bin \
	  $(IMAGE)

.PHONY: docker-run-gpu-nvidia
docker-run-gpu-nvidia: ## Run with NVIDIA GPU via nvidia-container-toolkit.
	docker run --rm -p 8080:8080 --runtime=nvidia --gpus all \
	  -v $$(realpath $${WHISPER_MODEL_PATH:-./ggml-base.bin}):/models/ggml-base.bin:ro \
	  -e WHISPER_MODEL_PATH=/models/ggml-base.bin \
	  $(IMAGE)

# ---- Kustomize --------------------------------------------------------------

KUSTOMIZE ?= kubectl
K8S_BASE   := deploy/k8s/base
K8S_DEV    := deploy/k8s/overlays/dev
K8S_PROD   := deploy/k8s/overlays/prod

.PHONY: kustomize-build-base
kustomize-build-base: ## Render the base manifests.
	$(KUSTOMIZE) kustomize $(K8S_BASE)

.PHONY: kustomize-build-dev
kustomize-build-dev: ## Render the dev overlay.
	$(KUSTOMIZE) kustomize $(K8S_DEV)

.PHONY: kustomize-build-prod
kustomize-build-prod: ## Render the prod overlay.
	$(KUSTOMIZE) kustomize $(K8S_PROD)

.PHONY: kustomize-dry-prod
kustomize-dry-prod: ## Server-side dry-run apply of the prod overlay.
	$(KUSTOMIZE) kustomize $(K8S_PROD) | kubectl apply --dry-run=server -f -

.PHONY: apply-prod
apply-prod: ## Apply the prod overlay to the current kube-context.
	$(KUSTOMIZE) apply -k $(K8S_PROD)