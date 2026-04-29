pushd %~dp0\zebra-crosslink\

$env:RUSTFLAGS="-Awarnings"
cargo run -F viz_gui --profile release-debug

popd