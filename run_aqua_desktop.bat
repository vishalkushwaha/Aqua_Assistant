@echo off
title Launch Aqua Desktop Assistant
cd /d "%~dp0"
set AQUA_WORKSPACE_ROOT=%~dp0
if "%AQUA_WORKSPACE_ROOT:~-1%"=="\" set AQUA_WORKSPACE_ROOT=%AQUA_WORKSPACE_ROOT:~0,-1%

echo ========================================================
echo   Launching Aqua Desktop Assistant and Admin Panel
echo ========================================================
echo Workspace: %AQUA_WORKSPACE_ROOT%
echo Starting Aqua Release Pipeline...
echo.

powershell -ExecutionPolicy Bypass -File "%~dp0scripts\start-aqua-release.ps1" -KeepAlive
pause
