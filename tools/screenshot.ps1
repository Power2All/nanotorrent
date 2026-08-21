# Captures a NanoTorrent window to a PNG, for verifying that dialogs actually
# render as intended instead of assuming they do from reading the code.
#
# This exists because the Win32 UI has already shipped a bug that ONLY a
# screenshot could catch: a checkbox that was correctly disabled via
# EnableWindow still painted itself as live, because darkmode.rs owner-draws
# checkboxes and was hardcoding the enabled theme parts. The code read fine.
#
# Usage:
#   tools\screenshot.ps1 -List
#   tools\screenshot.ps1 -Title 'Preferences' -Out shots\prefs.png
#   tools\screenshot.ps1 -Title 'NanoTorrent' -Out shots\main.png
#   tools\screenshot.ps1 -Screen -Out shots\context-menu.png
#
# Kept ASCII-only so Windows PowerShell 5.1, which reads .ps1 as the system
# codepage, parses it (same constraint as installer/make-assets.ps1).

param(
    # Substring (case-insensitive) of the window title to capture.
    [string]$Title,
    # Restrict the search to one process id. Defaults to any nanotorrent.exe.
    [int]$ProcessId = 0,
    [string]$Out = 'shot.png',
    # Full-screen grab instead of a per-window one. Needed for popup menus:
    # they are separate #32768 windows that PrintWindow renders unreliably.
    [switch]$Screen,
    # Render via PrintWindow instead of reading screen pixels. Survives being
    # occluded, but see the warning at the call site before trusting it.
    [switch]$PrintWindow,
    # Print every visible top-level window of the target process and exit.
    [switch]$List
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

if (-not ('NtShot' -as [type])) {
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class NtShot {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  // CharSet MUST be Unicode. Without it the marshaller picks the ANSI entry
  // point but still hands back a UTF-16 buffer, and every title comes back
  // one character long ("N" for "NanoTorrent") - which silently breaks the
  // title match rather than erroring.
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr ctx);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
}

# MUST run before any GetWindowRect call. NanoTorrent is only system-DPI-aware
# (main.rs calls SetProcessDPIAware), so on a multi-monitor setup where it sits
# on a higher-DPI display Windows virtualises it: the app reports a 573px-wide
# dialog that actually occupies 1127 physical pixels. A capture process that is
# NOT dpi-aware gets the virtualised numbers too, so it sizes the bitmap at 573
# and silently cuts the dialog in half - which reads exactly like a layout bug
# in the app and is not one. PER_MONITOR_AWARE_V2 (-4) makes the coordinates
# below physical, so they match what is actually on screen.
[void][NtShot]::SetProcessDpiAwarenessContext([IntPtr](-4))

function Get-TargetWindows {
    param([int]$Pid_)

    $found = New-Object System.Collections.ArrayList
    $cb = [NtShot+EnumProc]{
        param($h, $l)
        $owner = 0
        [void][NtShot]::GetWindowThreadProcessId($h, [ref]$owner)
        if ($owner -eq $script:WantPid -and [NtShot]::IsWindowVisible($h)) {
            $sb = New-Object System.Text.StringBuilder 512
            [void][NtShot]::GetWindowTextW($h, $sb, 512)
            $cn = New-Object System.Text.StringBuilder 256
            [void][NtShot]::GetClassNameW($h, $cn, 256)
            [void]$script:Found.Add([pscustomobject]@{
                Handle = $h
                Title  = $sb.ToString()
                Class  = $cn.ToString()
            })
        }
        return $true
    }
    $script:WantPid = $Pid_
    $script:Found = $found
    [void][NtShot]::EnumWindows($cb, [IntPtr]::Zero)
    return $found
}

# --- resolve the target process -------------------------------------------
# A whole-screen grab has no target, so do not demand one - requiring a running
# nanotorrent.exe to photograph a popup menu is exactly backwards.
if ($Screen -and $ProcessId -eq 0) {
    $ProcessId = $PID
}
if ($ProcessId -eq 0) {
    $proc = Get-Process nanotorrent -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $proc) { throw "nanotorrent.exe is not running (start it, or pass -ProcessId)" }
    $ProcessId = $proc.Id
}

if ($List) {
    Get-TargetWindows -Pid_ $ProcessId | Format-Table Handle, Class, Title -AutoSize
    return
}

# --- make sure the output directory exists --------------------------------
$outDir = Split-Path -Parent $Out
if ($outDir -and -not (Test-Path $outDir)) {
    [void](New-Item -ItemType Directory -Force -Path $outDir)
}

if ($Screen) {
    # Whole virtual desktop. Only for popup menus - for anything with an HWND
    # of its own, PrintWindow below is correct and far more reproducible.
    Add-Type -AssemblyName System.Windows.Forms
    $b = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($b.X, $b.Y, 0, 0, $bmp.Size)
    $g.Dispose()
    $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host "saved $Out (full screen)"
    return
}

if (-not $Title) { throw "pass -Title (or -List to see what is open, or -Screen)" }

$win = Get-TargetWindows -Pid_ $ProcessId |
    Where-Object { $_.Title -like "*$Title*" } |
    Select-Object -First 1

if (-not $win) {
    $open = (Get-TargetWindows -Pid_ $ProcessId | ForEach-Object { "'$($_.Title)'" }) -join ', '
    throw "no visible window matching '$Title'. Open windows: $open"
}

[void][NtShot]::SetForegroundWindow($win.Handle)
Start-Sleep -Milliseconds 300

$r = New-Object NtShot+RECT
[void][NtShot]::GetWindowRect($win.Handle, [ref]$r)
$w = $r.R - $r.L
$h = $r.B - $r.T
if ($w -le 0 -or $h -le 0) { throw "window '$($win.Title)' has no size" }

$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)

if ($PrintWindow) {
    # Immune to z-order because it asks the window to draw itself, but it draws
    # at the window's OWN scale. Against a DPI-virtualised app that is the
    # virtual size, not the physical one, so the render lands in the corner of a
    # correctly-sized bitmap. Only reach for this when the target is occluded
    # and you have confirmed app and capture agree on scale.
    $hdc = $g.GetHdc()
    $ok = [NtShot]::PrintWindow($win.Handle, $hdc, 2)
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    if (-not $ok) { $bmp.Dispose(); throw "PrintWindow failed for '$($win.Title)'" }
} else {
    # Default: read the actual pixels. Since this process is per-monitor DPI
    # aware the rect is physical, so the crop matches what a person sees -
    # which is the entire point of capturing rather than assuming. The window
    # was raised above, so z-order is only a hazard for something always-on-top.
    $g.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size)
    $g.Dispose()
}

# ponytail: GetWindowRect includes the invisible resize border on Win10+, so
# top-level captures carry a few px of margin. Harmless while it is consistent
# across the baseline; crop with DWMWA_EXTENDED_FRAME_BOUNDS if it ever matters.
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Host "saved $Out  ($w x $h)  <- '$($win.Title)'"
