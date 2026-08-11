@echo off
setlocal enabledelayedexpansion
cd /d "%~dp0"
set "ROOT=%~dp0.."

echo [1/5] Building release binary...
pushd "%ROOT%"
cargo build --release
if errorlevel 1 ( echo   cargo build failed & popd & exit /b 1 )
popd
if not exist "%ROOT%\target\release\nanotorrent.exe" (
  echo   nanotorrent.exe not found & exit /b 1
)

rem Optional: drop upx.exe next to this script to shrink the packaged exe.
echo [2/5] UPX compression...
if not exist "%~dp0upx.exe" (
  echo   skipped ^(no upx.exe in installer folder - get it from https://upx.github.io^)
) else (
  rem `upx -t` exits 0 on an already-packed file, 2 on an unpacked one.
  rem In practice packing breaks cargo's hardlink from target\release\deps, so
  rem the next build re-links a fresh unpacked exe and this never triggers -
  rem it is here so a leftover packed exe can't fail the build on AlreadyPacked.
  "%~dp0upx.exe" -qt "%ROOT%\target\release\nanotorrent.exe" >nul 2>&1
  if not errorlevel 1 (
    echo   already packed, leaving as is
  ) else (
    "%~dp0upx.exe" --best --lzma "%ROOT%\target\release\nanotorrent.exe"
    if errorlevel 1 ( echo   upx failed & exit /b 1 )
  )
)

echo [3/5] Generating installer assets from app.png and README.md...
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0make-assets.ps1"
if errorlevel 1 ( echo   asset generation failed & exit /b 1 )

echo [4/5] Locating NSIS makensis.exe...
set "MAKENSIS="
for %%P in (makensis.exe) do if not "%%~$PATH:P"=="" set "MAKENSIS=%%~$PATH:P"
if not defined MAKENSIS if exist "%ProgramFiles(x86)%\NSIS\makensis.exe" set "MAKENSIS=%ProgramFiles(x86)%\NSIS\makensis.exe"
if not defined MAKENSIS if exist "%ProgramFiles%\NSIS\makensis.exe" set "MAKENSIS=%ProgramFiles%\NSIS\makensis.exe"
if not defined MAKENSIS (
  echo   makensis.exe not found. Install NSIS 3.x from https://nsis.sourceforge.io
  exit /b 1
)

echo [5/5] Compiling installer with "!MAKENSIS!"...
"!MAKENSIS!" "%~dp0nanotorrent.nsi"
if errorlevel 1 ( echo   makensis failed & exit /b 1 )

echo.
rem The output name carries the version (${APP}-${VER}-Setup.exe in the .nsi),
rem so report whichever one was just written rather than a hardcoded name.
for /f "delims=" %%F in ('dir /b /o-d "%~dp0*-Setup.exe" 2^>nul') do (
  echo Done: %~dp0%%F
  goto :reported
)
echo Done ^(no *-Setup.exe found?^)
:reported
endlocal
