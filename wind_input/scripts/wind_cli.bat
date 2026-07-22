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

rem 无参数：显示顶层帮助（列出全部子命令）
if "%~1"=="" (
    "%TARGET%" help
    exit /b %errorlevel%
)

rem 已知子命令与帮助/版本旗标：原样透传
for %%s in (config schema dict phrase backup restart help --help -h --version -V) do (
    if "%~1"=="%%s" goto passthrough
)

rem 其余参数：向后兼容旧用法，自动补 config 前缀（如 `wind_cli get ui.theme.name`）
"%TARGET%" config %*
exit /b %errorlevel%

:passthrough
"%TARGET%" %*
exit /b %errorlevel%
