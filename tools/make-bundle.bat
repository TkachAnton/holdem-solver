@echo off
REM ============================================================
REM  make-bundle.bat -- Windows one-click wrapper
REM ============================================================
REM  Put this file next to make-bundle.ps1 (inside the tools folder),
REM  then run it from the PROJECT ROOT:
REM
REM      tools\make-bundle.bat
REM
REM  It packs the current folder into ONE .txt file and prints the
REM  full path of that file so you can attach it to the chat.
REM ============================================================

setlocal

set "HERE=%~dp0"
set "PROJ=%CD%"

echo Packing: %PROJ%
echo.

powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "& '%HERE%make-bundle.ps1' -Root '%PROJ%'"

endlocal
