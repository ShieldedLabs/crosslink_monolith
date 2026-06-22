@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0cargo_errorlimit.ps1" %*
exit /b %ERRORLEVEL%
