#!/usr/bin/env bash
set -euo pipefail

MODE="${1:-debug}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

cd "${SCRIPT_DIR}/zebra-crosslink"
export RUSTFLAGS="-Awarnings"

if [[ "${MODE}" == "release" ]]; then
  cargo run -F viz_gui --release
elif [[ "${MODE}" == "debug" ]]; then
  cargo run -F viz_gui
else
  echo "Usage: $0 [debug|release]"
  exit 1
fi
