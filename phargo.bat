@echo off

set "root=%~dp0"

set "RUSTFLAGS=-Awarnings"
set "SOURCE_DATE_EPOCH=0"

set "project=%1"

if "%project%"=="" ( echo No project specified! && exit /b 1 )
if "%project%"=="Debug" ( echo No project specified! && exit /b 1 )
if "%project%"=="Release" ( echo No project specified! && exit /b 1 )

set "config=%2"
set "platform=%3"
set "flags=-j 5"
if defined CARGO_BUILD_JOBS set "flags=-j %CARGO_BUILD_JOBS%"

if "%platform%"=="" set "platform=Win64"

if "%config%"=="Release" (
    set "flags=%flags% --release"
    set "build_folder=release"
) else (
    set "build_folder=debug"
)

if "%PH_SUBCOMMAND%"=="" ( echo PH_SUBCOMMAND not set, do not call this directly! && exit /b 1 )

if /i not "%platform%"=="Win64" if /i not "%platform%"=="Linux" if /i not "%platform%"=="APE" (
    echo Unknown platform "%platform%", expected Win64, Linux or APE! && exit /b 1
)

set "extra=%4 %5 %6 %7 %8 %9"

rem cosmo-build ignores CARGO_TARGET_DIR: the finished .com lands in the package's own
rem target\cosmo\<debug|release>, next to its per-architecture builds. On Windows cargo gives a
rem check the same build-script environment as a build, so cosmo-build cannot tell it to skip
rem the APE; anything but a build is told outright.
if /i "%platform%"=="APE" (
    set "flags=%flags% --features ape"
    if /i not "%PH_SUBCOMMAND%"=="build" set "COSMO_APE=0"
)

if /i "%platform%"=="Linux" goto :linux

set "launch=cargo"
set "PROTOC=%root%\protoc.exe"
goto :run

:linux

for /f "delims=" %%h in ('wsl -e printenv HOME') do set "lhome=%%h"
if "%lhome%"=="" ( echo Could not reach WSL! && exit /b 1 )

rem A non-interactive `wsl` invocation sources no shell profile, so rustup's PATH entry
rem never appears and cargo has to be named outright.
set "launch=wsl -e %lhome%/.cargo/bin/cargo"

wsl -e test -x %lhome%/.cargo/bin/cargo || goto :setup

rem Windows and Linux cannot share a project's target directory: what lives in it is
rem host-specific, so every switch between the two would rebuild the world. Linux output
rem goes on the WSL disk instead, which is also far faster to write than a /mnt/c path.
set "PROTOC=/usr/bin/protoc"
set "CARGO_TARGET_DIR=%lhome%/rust-targets/crosslink_monolith/%project%"
set "WSLENV=RUSTFLAGS:SOURCE_DATE_EPOCH:PROTOC:CARGO_TARGET_DIR"

:run

echo Pushing working dir: "%root%%project%" ^(%platform%^)
pushd "%root%%project%"
%launch% sweep --time 3
call "%root%cargo_errorlimit.bat" %launch% %PH_SUBCOMMAND% %flags% %extra%
set "result=%errorlevel%"
popd

if /i "%platform%"=="APE" if "%result%"=="0" if "%COSMO_APE%"=="" (
    for %%f in ("%root%%project%\target\cosmo\%build_folder%\*.com") do echo APE: %%~ff
    for /d %%d in ("%root%%project%\*") do for %%f in ("%%d\target\cosmo\%build_folder%\*.com") do echo APE: %%~ff
)

exit /b %result%

:setup

echo No Linux toolchain. One-time setup:
echo     wsl -e sudo apt-get install -y build-essential pkg-config curl clang libclang-dev protobuf-compiler libasound2-dev
echo     wsl -e bash -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"
echo     wsl -e %lhome%/.cargo/bin/cargo install cargo-sweep

exit /b 1
