@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"

if not exist "config.json" (
  echo [ERROR] Missing config.json
  echo [ERROR] Copy config.example.json to config.json first.
  pause
  exit /b 1
)

echo [INFO] Starting Cain Bot...
node src\index.mjs

echo.
echo [INFO] Bot exited with code: %errorlevel%
pause
