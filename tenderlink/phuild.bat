@echo off

set "RUSTFLAGS=-Awarnings"

rem cargo test handshake_test
cargo build
