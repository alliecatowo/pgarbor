#!/bin/sh
# Build the wasm demo into demo/web/pkg. Needs: rustup target wasm32-unknown-unknown,
# wasm-bindgen-cli 0.2.129 (cargo install wasm-bindgen-cli --version 0.2.129 --locked).
set -e
cd "$(dirname "$0")/.."
cargo build -p pgarbor-wasm --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir demo/web/pkg --out-name pgarbor_wasm \
  target/wasm32-unknown-unknown/release/pgarbor_wasm.wasm
echo "built demo/web/pkg; serve with: python3 -m http.server -d demo/web 8000"
