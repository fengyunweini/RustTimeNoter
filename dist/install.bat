@echo off
REM ===================================================================
REM  RustTimeNoter installer
REM  - Copies tracker.exe to %LOCALAPPDATA%\RustTimeNoter\bin\
REM  - Registers HKCU autostart (no admin needed)
REM  - Launches the daemon now
REM  - Opens the HTML report in your default browser
REM
REM  Uninstall: tracker uninstall autostart   (or run uninstall.bat)
REM ===================================================================
setlocal

set "EXE=%~dp0tracker.exe"
if not exist "%EXE%" (
    echo [error] tracker.exe not found next to this script.
    echo Expected: %EXE%
    pause
    exit /b 1
)

echo Installing RustTimeNoter for %USERNAME% ...
echo.
"%EXE%" setup
if errorlevel 1 (
    echo.
    echo [error] setup failed. See message above.
    pause
    exit /b 1
)

echo.
echo Installed. You can close this window.
pause
endlocal
