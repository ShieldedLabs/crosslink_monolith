@echo off
pushd %~dp0\zebra-crosslink\

set RUSTFLAGS=-Awarnings
cargo build -F viz_gui --release

popd