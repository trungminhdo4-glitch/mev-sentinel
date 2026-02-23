@echo off
REM MEV Sentinel - Launch Script
REM Sets up DLLTOOL env var (needed for first-time cargo build only) and runs the binary.

SET "PATH=C:\msys64\mingw64\bin;%USERPROFILE%\.cargo\bin;%PATH%"

cd /d "%~dp0mev-sentinel"

IF EXIST "target\debug\mev-sentinel.exe" (
    echo Starting MEV Sentinel...
    target\debug\mev-sentinel.exe
) ELSE (
    echo Binary not found. Building first...
    SET "DLLTOOL=C:\msys64\mingw64\bin\dlltool.exe"
    cargo build
    target\debug\mev-sentinel.exe
)
