@echo off
setlocal EnableExtensions
cd /d "%~dp0"

set "BIN=target\release\holdem-solver-server.exe"
set "DATA=%USERPROFILE%\holdem-solver-data"
set "PORT=8080"

if exist "%USERPROFILE%\.cargo\bin" set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

where cargo >nul 2>nul
if errorlevel 1 (
    echo [holdem] cargo not found. Install Rust from https://rustup.rs and rerun this file.
    pause
    exit /b 1
)

rem Always run an incremental release build: a few seconds when nothing changed,
rem and the only way to pick up frontend edits (UI is embedded into the exe).

:build
echo [holdem] Building the solver ^(incremental; first run takes minutes^)...
taskkill /f /im holdem-solver-server.exe >nul 2>nul
timeout /t 1 /nobreak >nul
cargo +1.75.0 build --release -p holdem-solver-server
if errorlevel 1 (
    echo [holdem] Build failed. See the compiler output above.
    pause
    exit /b 1
)

:run
if not exist "%BIN%" (
    echo [holdem] Binary missing: %BIN%
    pause
    exit /b 1
)

echo [holdem] Starting server on http://127.0.0.1:%PORT% ...
start /b "" "%BIN%" --bind 127.0.0.1:%PORT% --data-dir "%DATA%" --max-active-jobs 1

where curl >nul 2>nul
if errorlevel 1 goto openbrowser

:waitloop
timeout /t 1 /nobreak >nul
curl -s -o nul http://127.0.0.1:%PORT%/healthz
if errorlevel 1 goto waitloop

:openbrowser
start "" http://127.0.0.1:%PORT%/
echo [holdem] UI is open in your browser. Jobs are stored in: %DATA%
echo [holdem] Keep this window open while you work. Close it to stop the server.
echo [holdem] Full clean rebuild: stop-solver.bat, rmdir /s /q target, then run start-solver.bat
pause
