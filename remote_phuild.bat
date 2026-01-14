@echo off

set "bin=ShieldedLabs\crosslink_monolith\zebra-crosslink\target\debug\deps"

ssh Madina@madina-a16.shire-lydian.ts.net cmd /c \"C:\Work\ShieldedLabs\crosslink_monolith\server_phuild.bat %*\" || exit /b
REM copy /y "\\madina-a16.shire-lydian.ts.net\%bin%\zebrad.exe" "C:\Work\%bin%\zebrad.exe"                            || exit /b
REM copy /y "\\madina-a16.shire-lydian.ts.net\%bin%\zebrad.pdb" "C:\Work\%bin%\zebrad.pdb"                            || exit /b
