@echo off
pushd %~dp0\zebra-crosslink\

set RUSTFLAGS=-Awarnings
cargo run -F viz_gui -- -c D:\testnet.toml

popd