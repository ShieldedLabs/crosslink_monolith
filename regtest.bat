
@echo off

set "PROTOC=C:\Work\ShieldedLabs\protoc-33.1-win64\bin\protoc.exe"

cd C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\
cargo build -Fviz_gui

start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "regtestnet_1.toml"
start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "regtestnet_2.toml"
