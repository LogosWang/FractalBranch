# Phase 0 manual check. Shows the two candidate panels on the desktop for five minutes.
# Both exit on their own. Logs go to out\check-*.log.
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$out = Join-Path $here 'out'
New-Item -ItemType Directory -Force -Path $out | Out-Null
$exe = Join-Path $here '..\..\target\release\desktop-mount.exe'
$common = @('--seconds', '300', '--w', '1100', '--h', '500')
Start-Process -FilePath $exe -ArgumentList (@('--mode', 'progman-dcomp', '--warp', '--x', '120', '--y', '700', '--log', "`"$out\check-dcomp.log`"") + $common)
Start-Process -FilePath $exe -ArgumentList (@('--mode', 'popup-owned', '--software', '--x', '1320', '--y', '700', '--log', "`"$out\check-popup.log`"") + $common)
