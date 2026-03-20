@echo off
cargo check > "%TEMP%\cargo_check_out.txt" 2>&1
set CARGO_ERR=%ERRORLEVEL%
powershell -NoProfile -Command ^
  "$n=0; foreach($line in (Get-Content '%TEMP%\cargo_check_out.txt')){ if($line -match '^error'){$n++}; if($n -ge 2){break}; Write-Output $line }"
exit /b %CARGO_ERR%