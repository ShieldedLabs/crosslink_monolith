
@echo off

set "root=C:\Work\ShieldedLabs\crosslink_monolith\"

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
    set "executable=zebrad"
) else (
    set "executable=visualizer_zcash"
)

start "" "%root%\%project%\target\%build_folder%\deps\%executable%.exe" "-c" "%root%\%project%\regtest.local.toml"
