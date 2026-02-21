@echo off

rem if "%USERDOMAIN%"=="MADINA-A16" (
    C:\Work\ShieldedLabs\crosslink_monolith\server_phuild.bat %* & exit /b
rem )

ssh Madina@madina-a16.shire-lydian.ts.net cmd /c \"C:\Work\ShieldedLabs\crosslink_monolith\server_phuild.bat %*\" || exit /b

set "exe=ShieldedLabs\crosslink_monolith\zebra-gui\target\%1\visualizer_zcash"
REM set "exe=ShieldedLabs\crosslink_monolith\zebra-crosslink\target\%1\zebrad"

copy /y "\\madina-a16.shire-lydian.ts.net\%exe%.exe" "C:\Work\%exe%.exe"                            || exit /b
REM copy /y "\\madina-a16.shire-lydian.ts.net\%exe%.pdb" "C:\Work\%exe%.pdb"                            || exit /b
