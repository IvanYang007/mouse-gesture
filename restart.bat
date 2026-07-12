@echo off
taskkill /f /im mouse-gesture.exe >nul 2>&1
timeout /t 1 /nobreak >nul
start "" "%~dp0target\release\mouse-gesture.exe"
echo Mouse Gesture restarted
