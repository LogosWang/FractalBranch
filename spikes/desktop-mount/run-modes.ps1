# Show all mount modes on the desktop at once, trigger Show Desktop (same as Win+D) so the
# desktop is visible, take one screenshot, then restore the windows. User windows are
# minimized for about two seconds. Doubles as the Win+D survival test.
param(
    [string[]]$ExtraArgs = @(),
    [string]$Tag = 'all',
    [string[]]$Modes = @('child-layered', 'child-plain', 'child-dcomp', 'progman-layered', 'progman-dcomp', 'popup-owned')
)
$Modes = @($Modes | ForEach-Object { $_ -split ',' })
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$out = Join-Path $here 'out'
New-Item -ItemType Directory -Force -Path $out | Out-Null
$exe = Join-Path $here '..\..\target\release\desktop-mount.exe'

Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices;
public class Native {
  [DllImport("user32")] public static extern IntPtr SetProcessDpiAwarenessContext(IntPtr c);
  [DllImport("user32")] public static extern int GetSystemMetrics(int i);
}
'@
[void][Native]::SetProcessDpiAwarenessContext([IntPtr](-4))

$procs = @()
$i = 0
foreach ($m in $Modes) {
    $x = 120 + ($i % 2) * 1200
    $y = 120 + [math]::Floor($i / 2) * 620
    $log = Join-Path $out "$Tag-$m.log"
    $argList = @('--mode', $m, '--seconds', '9', '--x', $x, '--y', $y, '--w', 1100, '--h', 500, '--log', "`"$log`"") + $ExtraArgs
    $procs += Start-Process -FilePath $exe -ArgumentList $argList -PassThru
    $i++
}
Start-Sleep -Milliseconds 1500

$shell = New-Object -ComObject Shell.Application
$shell.ToggleDesktop()
try {
    Start-Sleep -Milliseconds 1200
    $sw = [Native]::GetSystemMetrics(0); $sh = [Native]::GetSystemMetrics(1)
    $bmp = New-Object Drawing.Bitmap $sw, $sh
    $g = [Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen(0, 0, 0, 0, $bmp.Size)
    $g.Dispose()
    $small = New-Object Drawing.Bitmap $bmp, ([int]($sw / 2)), ([int]($sh / 2))
    $small.Save((Join-Path $out "$Tag.png"))
    $small.Dispose(); $bmp.Dispose()
} finally {
    $shell.ToggleDesktop()
}
$procs | ForEach-Object { $_.WaitForExit() }
foreach ($m in $Modes) {
    "=== $m"
    Get-Content -LiteralPath (Join-Path $out "$Tag-$m.log") -Encoding UTF8 | Where-Object { $_ -notmatch ' > ' }
}
