@echo off
setlocal

if exist "%~dp0wind_input_dev.exe" (
    set "TARGET=%~dp0wind_input_dev.exe"
) else (
    set "TARGET=%~dp0wind_input.exe"
)

if not exist "%TARGET%" (
    echo wind_cli: target not found: %TARGET% 1>&2
    exit /b 127
)

if "%~1"=="config" (
    "%TARGET%" %*
) else (
    "%TARGET%" config %*
)

exit /b %errorlevel%
