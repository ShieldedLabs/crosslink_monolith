@echo off
setlocal

set "BASH_EXE="

rem Prefer Git for Windows explicitly. `where bash.exe` may resolve to the
rem Windows/WSL launcher, which is not necessarily Git Bash.
if exist "%ProgramFiles%\Git\bin\bash.exe" set "BASH_EXE=%ProgramFiles%\Git\bin\bash.exe"
if not defined BASH_EXE if exist "%ProgramFiles%\Git\usr\bin\bash.exe" set "BASH_EXE=%ProgramFiles%\Git\usr\bin\bash.exe"
if not defined BASH_EXE if exist "%LOCALAPPDATA%\Programs\Git\bin\bash.exe" set "BASH_EXE=%LOCALAPPDATA%\Programs\Git\bin\bash.exe"
if not defined BASH_EXE if exist "%LOCALAPPDATA%\Programs\Git\usr\bin\bash.exe" set "BASH_EXE=%LOCALAPPDATA%\Programs\Git\usr\bin\bash.exe"

if not defined BASH_EXE (
  echo error: Git for Windows bash.exe was not found 1>&2
  exit /b 1
)

"%BASH_EXE%" "%~dp0sync-crosslink-book.sh" %*
exit /b %errorlevel%
