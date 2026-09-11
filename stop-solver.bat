@echo off
taskkill /f /im holdem-solver-server.exe >nul 2>nul
if errorlevel 1 (echo [holdem] No running solver server found.) else (echo [holdem] Solver server stopped.)
pause
