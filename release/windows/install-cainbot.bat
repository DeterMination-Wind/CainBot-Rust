@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"
powershell -ExecutionPolicy Bypass -File "%~dp0install-cainbot.ps1"
set "EXIT_CODE=%errorlevel%"
echo.
if not "%EXIT_CODE%"=="0" (
  echo [ERROR] 安装失败，退出码 %EXIT_CODE%
) else (
  echo [INFO] 安装脚本已执行完成。
)
pause
exit /b %EXIT_CODE%
