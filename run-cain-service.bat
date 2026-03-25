@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"

set "NAPCAT_DIR=%~dp0..\NapCat.Shell"
if not exist "%NAPCAT_DIR%\launcher.bat" (
  echo [ERROR] Missing NapCat.Shell\launcher.bat
  echo [ERROR] Expected path: %NAPCAT_DIR%
  pause
  exit /b 1
)

echo [INFO] Starting NapCat.Shell...
start "NapCat.Shell" /D "%NAPCAT_DIR%" cmd /c "call \"%NAPCAT_DIR%\launcher.bat\" -q 1705087729"

echo [INFO] Waiting for NapCat to initialize...
timeout /t 8 /nobreak >nul

echo [INFO] Starting Cain Bot...
call "%~dp0run-cain-bot.bat"
