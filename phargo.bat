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

if "%PH_SUBCOMMAND%"=="" ( echo PH_SUBCOMMAND not set, do not call this directly! && exit /b 1 )

set "extra=%3 %4 %5 %6 %7 %8 %9"

echo Pushing working dir: "%root%%project%"
pushd "%root%%project%"
cargo sweep --time 3
call "%root%cargo_errorlimit.bat" %PH_SUBCOMMAND% %flags% %extra%
set "result=%errorlevel%"
popd

exit /b %result%
