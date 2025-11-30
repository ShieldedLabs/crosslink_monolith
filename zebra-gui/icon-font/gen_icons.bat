@echo off

echo Copying from folder: "%1"

echo config.json...
copy "%1\config.json" ".\config.json"

echo fontello.ttf...
copy "%1\font\fontello.ttf" "..\assets\fontello.ttf"

echo Sorting config.json...
type config.json | jq-win64.exe --from-file gen_sorted_config.jq > config_sorted.json
type config_sorted.json > config.json

echo Deleting config_sorted.json...
del config_sorted.json

echo Generating C header...
type config.json | jq-win64.exe --from-file gen_header.jq --raw-output > ..\src\fontello_icons.rs

pause
