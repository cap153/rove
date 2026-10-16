@echo off
set ROVE_LOG=1
cd /d %~dp0
target\x86_64-pc-windows-gnu\release\rove.exe %*
