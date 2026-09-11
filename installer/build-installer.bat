@echo off
setlocal enabledelayedexpansion
cd /d "%~dp0"
set "ROOT=%~dp0.."

echo [1/5] Building release binary...
pushd "%ROOT%"
cargo build --release
if errorlevel 1 ( echo   cargo build failed & popd & exit /b 1 )
popd
if not exist "%ROOT%\target\release\nanotorrent-gui.exe" (
  echo   nanotorrent-gui.exe not found & exit /b 1
)
if not exist "%ROOT%\target\release\nanotorrent-cli.exe" (
  echo   nanotorrent-cli.exe not found & exit /b 1
)

rem UPX packing is OFF. It is opt-in: set NANOTORRENT_UPX=1 and drop upx.exe
rem next to this script.
rem
rem A packed binary is 16 MB on disk that has to allocate and decompress 64 MB
rem before the loader has finished, on every single launch. A freshly installed
rem 0.3.7 failed exactly there twice - an access violation inside ntdll, which
rem reaches the user as "The application was unable to start correctly
rem (0xc0000142)" and cannot be debugged from a stack trace. It has not been
rem reproduced since, so this is not a proven verdict on UPX; it is a bad trade
rem regardless, and a smaller one than it looks. NSIS already compresses the
rem payload itself (SetCompressor /SOLID lzma below), so packing first only
rem saves a couple of MB of DOWNLOAD - measured, 0.3.6 packed was 14.1 MB
rem against 0.3.7 unpacked at 16.7 MB. The ~47 MB is installed footprint, not
rem bandwidth. Against that: a slower start on every launch, a standing
rem antivirus heuristic flag, and a class of loader failure with no stack
rem trace. The MSIX is the same story - it is a compressed container, so
rem packing inside it just hands the packager incompressible data.
echo [2/5] UPX compression...
if not "%NANOTORRENT_UPX%"=="1" (
  echo   skipped ^(off by default - set NANOTORRENT_UPX=1 to pack^)
) else if not exist "%~dp0upx.exe" (
  echo   skipped ^(NANOTORRENT_UPX=1 but no upx.exe in installer folder^)
) else (
  rem Both binaries: the GUI is the bulk of the download, and the launcher
  rem ships beside it twice - once under its own name and once as
  rem nanotorrent.exe - so it is worth packing too.
  rem
  rem `upx -t` exits 0 on an already-packed file, 2 on an unpacked one.
  rem In practice packing breaks cargo's hardlink from target\release\deps, so
  rem the next build re-links a fresh unpacked exe and this never triggers -
  rem it is here so a leftover packed exe can't fail the build on AlreadyPacked.
  for %%B in (nanotorrent-gui.exe nanotorrent-cli.exe) do (
    "%~dp0upx.exe" -qt "%ROOT%\target\release\%%B" >nul 2>&1
    if not errorlevel 1 (
      echo   %%B already packed, leaving as is
    ) else (
      "%~dp0upx.exe" --best --lzma "%ROOT%\target\release\%%B"
      if errorlevel 1 ( echo   upx failed on %%B & exit /b 1 )
    )
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
