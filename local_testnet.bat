
@echo off

set "RUSTFLAGS=-Awarnings"
set "PROTOC=C:\Work\ShieldedLabs\protoc-33.1-win64\bin\protoc.exe"

pushd C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\
cargo build -Fviz_gui
REM && start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "testnet_1.toml" && start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "testnet_2.toml"

REM "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\testnet_1.toml"
REM "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\testnet_2.toml"

popd
