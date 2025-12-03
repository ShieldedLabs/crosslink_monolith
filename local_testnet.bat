
@echo off

set "RUSTFLAGS=-Awarnings"
set "PROTOC=C:\Work\ShieldedLabs\protoc-33.1-win64\bin\protoc.exe"
set "SOURCE_DATE_EPOCH=0"

REM set

pushd C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\
cargo build -Fviz_gui
copy /Y "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad_testnet_1.exe"
copy /Y "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad_testnet_2.exe"

REM && start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\testnet_1.toml" && start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\testnet_2.toml"

REM cargo build --release -Fviz_gui && copy /Y "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\release\deps\zebrad.exe" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\release\deps\zebrad_testnet_1.exe" && copy /Y "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\release\deps\zebrad.exe" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\release\zebrad_testnet_2.exe"
REM start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\release\deps\zebrad.exe" "-c" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\testnet_1.toml" && start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\release\deps\zebrad.exe" "-c" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\testnet_2.toml"

REM "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\testnet_1.toml"
REM "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps\zebrad.exe" "-c" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\testnet_2.toml"

REM start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\release\deps\zebrad.exe" "-c" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\testnet_1.toml"
REM start "" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\target\release\deps\zebrad.exe" "-c" "C:\Work\ShieldedLabs\crosslink_monolith\zebra-crosslink\testnet_2.toml"

popd

REM exit /b 1
