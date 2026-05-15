@echo off

set "root=C:\Work\ShieldedLabs\crosslink_monolith\"

set "RUSTFLAGS=-Awarnings"
set "PROTOC=C:\Work\ShieldedLabs\crosslink_monolith\protoc.exe"
set "SOURCE_DATE_EPOCH=0"

if "%1"=="-Release" (
    set "flags=--release"
) else (
    set "flags="
)

cargo build %flags%
