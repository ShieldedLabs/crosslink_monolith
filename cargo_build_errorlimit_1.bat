@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0cargo_build_errorlimit_1.ps1" %*
exit /b %ERRORLEVEL%
