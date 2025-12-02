
@echo off

set "PROTOC=C:\Work\ShieldedLabs\protoc-33.1-win64\bin\protoc.exe"

pushd C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\
cargo build --release -Fviz_gui && start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "testnet_1.toml" && start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "testnet_2.toml"
popd
