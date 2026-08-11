@echo off
setlocal enabledelayedexpansion
cd /d "%~dp0"
set "ROOT=%~dp0.."

echo [1/4] Building release binary...
pushd "%ROOT%"
cargo build --release
if errorlevel 1 ( echo   cargo build failed & popd & exit /b 1 )
popd
if not exist "%ROOT%\target\release\nanotorrent.exe" (
  echo   nanotorrent.exe not found & exit /b 1
)

echo [2/4] Generating installer assets from app.png and README.md...
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0make-assets.ps1"
if errorlevel 1 ( echo   asset generation failed & exit /b 1 )

echo [3/4] Locating NSIS makensis.exe...
set "MAKENSIS="
for %%P in (makensis.exe) do if not "%%~$PATH:P"=="" set "MAKENSIS=%%~$PATH:P"
if not defined MAKENSIS if exist "%ProgramFiles(x86)%\NSIS\makensis.exe" set "MAKENSIS=%ProgramFiles(x86)%\NSIS\makensis.exe"
if not defined MAKENSIS if exist "%ProgramFiles%\NSIS\makensis.exe" set "MAKENSIS=%ProgramFiles%\NSIS\makensis.exe"
if not defined MAKENSIS (
  echo   makensis.exe not found. Install NSIS 3.x from https://nsis.sourceforge.io
  exit /b 1
)

echo [4/4] Compiling installer with "!MAKENSIS!"...
"!MAKENSIS!" "%~dp0nanotorrent.nsi"
if errorlevel 1 ( echo   makensis failed & exit /b 1 )

echo.
echo Done: %~dp0NanoTorrent-Setup.exe
endlocal
