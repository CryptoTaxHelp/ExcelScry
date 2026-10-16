# Capture a screenshot of a running Windows app's own window.
#
# Used to verify the built application visually: it captures only the target
# window's rectangle, not the whole desktop.
#
#   powershell -File scripts/capture-window.ps1 -ProcessName ExcelScry -Out shot.png

param(
    [Parameter(Mandatory = $true)][string]$ProcessName,
    [Parameter(Mandatory = $true)][string]$Out
)

$ErrorActionPreference = "Stop"

Add-Type @"
using System;
using System.Runtime.InteropServices;

[StructLayout(LayoutKind.Sequential)]
public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }

public class WinCapture {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(
        IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@

# 0x0001 = SWP_NOSIZE, 0x0002 = SWP_NOMOVE, 0x0040 = SWP_SHOWWINDOW.
# Without a topmost nudge, SetForegroundWindow is often refused (Windows blocks
# foreground stealing), and the capture shows whatever window was on top.
$SWP_NOSIZE_NOMOVE = 0x0003 -bor 0x0040
$HWND_TOPMOST = [IntPtr](-1)
$HWND_NOTOPMOST = [IntPtr](-2)

# Without this, PowerShell reports DPI-virtualised coordinates while the
# capture happens in physical pixels, so the shot lands offset from the window.
[WinCapture]::SetProcessDPIAware() | Out-Null

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

$proc = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue |
    Where-Object { $_.MainWindowHandle -ne 0 } |
    Select-Object -First 1

if (-not $proc) {
    Write-Error "No window found for process '$ProcessName'."
    exit 1
}

$handle = $proc.MainWindowHandle
[WinCapture]::ShowWindow($handle, 9) | Out-Null   # SW_RESTORE

# Move the window fully inside the work area first. A window hanging off the
# bottom of the screen is not composited, so the captured region would contain
# unrendered black instead of the application.
$work = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
$rect0 = New-Object RECT
[WinCapture]::GetWindowRect($handle, [ref]$rect0) | Out-Null
$w0 = [Math]::Min($rect0.Right - $rect0.Left, $work.Width)
$h0 = [Math]::Min($rect0.Bottom - $rect0.Top, $work.Height)
[WinCapture]::SetWindowPos($handle, $HWND_TOPMOST, $work.X, $work.Y, $w0, $h0, 0x0040) | Out-Null

[WinCapture]::SetWindowPos($handle, $HWND_TOPMOST, 0, 0, 0, 0, $SWP_NOSIZE_NOMOVE) | Out-Null
[WinCapture]::SetForegroundWindow($handle) | Out-Null
# Moving the window exposes new pixels; the compositor needs time to repaint
# or the capture catches a stale/torn frame. Warm up with a discarded shot.
Start-Sleep -Milliseconds 900
$warm = New-Object System.Drawing.Bitmap(8, 8)
$wg = [System.Drawing.Graphics]::FromImage($warm)
$wg.CopyFromScreen(0, 0, 0, 0, $warm.Size)
$wg.Dispose(); $warm.Dispose()
Start-Sleep -Milliseconds 700
[WinCapture]::SetWindowPos($handle, $HWND_NOTOPMOST, 0, 0, 0, 0, $SWP_NOSIZE_NOMOVE) | Out-Null
Start-Sleep -Milliseconds 400

$rect = New-Object RECT
[WinCapture]::GetWindowRect($handle, [ref]$rect) | Out-Null

$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top
if ($width -le 0 -or $height -le 0) {
    Write-Error "Window has no size ($width x $height)."
    exit 1
}

$bitmap = New-Object System.Drawing.Bitmap($width, $height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bitmap.Size)
$bitmap.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$graphics.Dispose()
$bitmap.Dispose()

Write-Output "captured ${width}x${height} at $($rect.Left),$($rect.Top) -> $Out"
