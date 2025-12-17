
@echo off

set "root=C:\Work\ShieldedLabs\crosslink_monolith\"

set "RUSTFLAGS=-Awarnings"
set "PROTOC=C:\Work\ShieldedLabs\protoc-33.1-win64\bin\protoc.exe"
set "SOURCE_DATE_EPOCH=0"

set "project=zebra-crosslink"
REM set "project=zebra-gui"

set "config=%1"

if "%config%"=="-Release" (
    set "flags=--release"
    set "build_folder=release"
) else (
    set "flags="
    set "build_folder=debug"
)

if "%project%"=="zebra-crosslink" (
    set "flags=%flags% -Fviz_gui"
)

pushd %root%\%project%\
cargo build %flags%
popd

if %errorlevel% neq 0 exit /b %errorlevel%

if "%project%"=="zebra-crosslink" (
    echo copy /Y  "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad.exe"                "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad_testnet_1.exe"
    echo copy /Y  "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad.exe"                "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad_testnet_2.exe"
    echo start "" "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad_testnet_1.exe" "-c" "%root%\zebra-crosslink\testnet_1.toml"
    echo start "" "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad_testnet_2.exe" "-c" "%root%\zebra-crosslink\testnet_2.toml"
)
