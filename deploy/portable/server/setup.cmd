@echo off
"%~dp0inori-server.exe" setup %*
if errorlevel 1 pause
