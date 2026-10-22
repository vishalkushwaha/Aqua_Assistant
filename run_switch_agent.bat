@echo off
title Launch Aqua Company Brain Switch Agent
cd /d "%~dp0"
set AQUA_WORKSPACE_ROOT=%~dp0
if "%AQUA_WORKSPACE_ROOT:~-1%"=="\" set AQUA_WORKSPACE_ROOT=%AQUA_WORKSPACE_ROOT:~0,-1%

echo ========================================================
echo   Launching Aqua Company Brain Switch Agent
echo ========================================================
echo Workspace: %AQUA_WORKSPACE_ROOT%
echo.

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\run-switch-agent.ps1"
pause
