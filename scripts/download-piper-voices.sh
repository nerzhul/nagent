#!/usr/bin/env bash
# Download Piper ONNX voice checkpoints from the official Rhasspy Piper
# releases on GitHub.
#
# Usage:
#   scripts/download-piper-voices.sh [voice_id ...]
#   scripts/download-piper-voices.sh en_US-lessac-medium fr_FR-upmc-medium
#
# With no arguments, downloads the two defaults used by the server's
# `[tts]` config (en_US-lessac-medium, fr_FR-upmc-medium).
#
# Writes each voice as `<id>.onnx` + `<id>.onnx.json` into
# $TTS_MODEL_DIR (default `./models/piper`). The script is idempotent:
# it skips voices whose `.onnx` file is already on disk.
#
# Voice catalogue:
#   https://huggingface.co/rhasspy/piper-voices/tree/main
#
# License: every voice distributed by Piper is CC-BY-NC-SA (some are
# MIT — see the catalogue page for the per-voice LICENSE). The
# downstream user is responsible for honouring attribution and the
# non-commercial clause. See README "Voice attribution & license".

set -euo pipefail

# Default voices match `TtsConfig::default` in
# `crates/stt-server/src/config.rs`.
DEFAULT_VOICES=(en_US-lessac-medium fr_FR-upmc-medium)

REPO="rhasspy/piper"
# This asset is a single tarball containing every Piper voice at the
# release tag — we extract only the voices we need, so we never write
# 30 GB of unwanted voices to disk.
RELEASE_ASSET_BASE="https://github.com/${REPO}/releases/download"

# Pin to a specific Piper release so the script is reproducible. Bump
# together with the version documented in the README.
PIPER_RELEASE="2023.11.14-2"

DEST_DIR="${TTS_MODEL_DIR:-./models/piper}"

if [ "$#" -gt 0 ]; then
  VOICES=("$@")
else
  VOICES=("${DEFAULT_VOICES[@]}")
fi

mkdir -p "${DEST_DIR}"

# The full Piper release asset is heavy (~25 GB). Download once into a
# scratch directory, extract only the voices we need, then clean up.
SCRATCH="$(mktemp -d -t piper-voices.XXXXXX)"
trap 'rm -rf "${SCRATCH}"' EXIT

ASSET_URL="${RELEASE_ASSET_BASE}/${PIPER_RELEASE}/piper_voices.tar.gz"
TARBALL="${SCRATCH}/piper_voices.tar.gz"

echo "Downloading Piper voices release ${PIPER_RELEASE}…" >&2
echo "  (asset: ${ASSET_URL})" >&2
if command -v curl >/dev/null 2>&1; then
  curl -fL --retry 3 -o "${TARBALL}" "${ASSET_URL}"
elif command -v wget >/dev/null 2>&1; then
  wget -O "${TARBALL}" "${ASSET_URL}"
else
  echo "error: need curl or wget on PATH" >&2
  exit 1
fi

echo "Extracting ${#VOICES[@]} voice(s) into ${DEST_DIR}…" >&2
for voice in "${VOICES[@]}"; do
  if [ -f "${DEST_DIR}/${voice}.onnx" ]; then
    echo "  - ${voice}: already present, skipping" >&2
    continue
  fi
  # Piper release tarball layout: <lang>/<voice>/<voice>.onnx{,.json}
  # Find the voice inside the tarball by listing entries first.
  MATCH="$(tar tzf "${TARBALL}" \
    | grep -E "/${voice}\.onnx(\.json)?\$" || true)"
  if [ -z "${MATCH}" ]; then
    echo "  - ${voice}: not present in release tarball (typo?)" >&2
    exit 1
  fi
  tar -C "${DEST_DIR}" -xzf "${TARBALL}" ${MATCH}
done

echo "Done. Voices in ${DEST_DIR}:" >&2
ls -1 "${DEST_DIR}" | grep '\.onnx$' | sed 's/^/  - /' >&2
