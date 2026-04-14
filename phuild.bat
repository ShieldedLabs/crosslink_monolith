@echo off

set "root=%~dp0"

set "RUSTFLAGS=-Awarnings"
set "PROTOC=%root%\protoc.exe"
set "SOURCE_DATE_EPOCH=0"

set "project=%1"

if "%project%"=="" ( echo No project specified! && exit /b 1 )
if "%project%"=="Debug" ( echo No project specified! && exit /b 1 )
if "%project%"=="Release" ( echo No project specified! && exit /b 1 )

set "config=%2"
set "flags=-j 5"

if "%config%"=="Release" (
    set "flags=%flags% --release"
    set "build_folder=release"
) else (
    set "build_folder=debug"
)

if "%project%"=="zebra-crosslink" (
    set "flags=%flags% -Fviz_gui"
)

echo Pushing working dir: "%root%/%project%"
pushd "%root%/%project%"
cargo sweep --time 3
%root%/cargo_build_errorlimit_1 %flags%
popd

if %errorlevel% neq 0 exit /b %errorlevel%

if "%project%"=="zebra-crosslink" (
    del /Q /F "%root%\zebra-crosslink\ZZ_1"
    rem copy /Y  "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad.exe"                "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad_testnet_1.exe"
    rem copy /Y  "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad.exe"                "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad_testnet_2.exe"
    rem start "" "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad_testnet_1.exe" "-c" "%root%\zebra-crosslink\testnet_1.toml"
    rem start "" "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad_testnet_2.exe" "-c" "%root%\zebra-crosslink\testnet_2.toml"
    rem start "" "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad.exe" "-c" "%root%\zebra-crosslink\.AA_0.local.toml"
    rem start "" "%root%\zebra-crosslink\target\%build_folder%\deps\zebrad.exe" "-c" "%root%\zebra-crosslink\.AA_1.local.toml"
)
