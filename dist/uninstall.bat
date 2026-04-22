@echo off
REM ===================================================================
REM  RustTimeNoter uninstaller
REM  - Stops the daemon (graceful)
REM  - Removes HKCU autostart entry
REM  - Leaves your data files intact at %LOCALAPPDATA%\RustTimeNoter
REM    (delete that folder by hand if you also want to wipe data)
REM ===================================================================
setlocal

set "BIN=%LOCALAPPDATA%\RustTimeNoter\bin\tracker.exe"
if not exist "%BIN%" (
    echo Nothing to uninstall (no tracker.exe at %BIN%).
    pause
    exit /b 0
)

echo Stopping daemon ...
"%BIN%" stop
echo.
echo Removing autostart entry ...
"%BIN%" uninstall autostart

echo.
echo Done. Data folder preserved:
echo   %LOCALAPPDATA%\RustTimeNoter
echo Delete it manually if you also want to wipe collected data.
pause
endlocal
