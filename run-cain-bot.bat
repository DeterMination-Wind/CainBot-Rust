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

set "CAINBOT_EXE=%~dp0target\release\cainbot-rs.exe"
if not exist "%CAINBOT_EXE%" (
  echo [INFO] Rust binary not found, building release binary...
  cargo build --release --bin cainbot-rs
  if errorlevel 1 (
    echo [ERROR] Rust build failed.
    pause
    exit /b 1
  )
)

set "CAINBOT_CONFIG=%~dp0config.json"
echo [INFO] Starting Cain Bot (Rust runtime)...
"%CAINBOT_EXE%"

echo.
echo [INFO] Bot exited with code: %errorlevel%
pause
