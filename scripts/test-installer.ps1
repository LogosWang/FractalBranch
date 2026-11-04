# End-to-end installer test on this machine: install through the real dialogs, uninstall,
# reinstall, and a first run without any previous data. Leaves the app installed and running.
# The user's layout is backed up first and compared at the end.
param([string]$Setup, [string]$Backup)
Add-Type @'
using System;using System.Text;using System.Runtime.InteropServices;using System.Collections.Generic;
public class T{
public delegate bool E(IntPtr h,IntPtr l);
[DllImport("user32",CharSet=CharSet.Unicode)]public static extern IntPtr FindWindow(string c,string t);
[DllImport("user32",CharSet=CharSet.Unicode)]public static extern IntPtr FindWindowEx(IntPtr p,IntPtr a,string c,string t);
[DllImport("user32")]public static extern bool EnumWindows(E e,IntPtr l);
[DllImport("user32")]public static extern bool EnumChildWindows(IntPtr p,E e,IntPtr l);
[DllImport("user32",CharSet=CharSet.Unicode)]public static extern int GetClassName(IntPtr h,StringBuilder s,int n);
[DllImport("user32",CharSet=CharSet.Unicode)]public static extern int GetWindowText(IntPtr h,StringBuilder s,int n);
[DllImport("user32")]public static extern bool PostMessage(IntPtr h,uint m,IntPtr w,IntPtr l);
[DllImport("user32")]public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32")]public static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
public static string C(IntPtr h){var c=new StringBuilder(64);GetClassName(h,c,64);return c.ToString();}
public static IntPtr Box(uint pid){IntPtr f=IntPtr.Zero;EnumWindows((h,x)=>{uint p;GetWindowThreadProcessId(h,out p);if(p==pid&&C(h)=="#32770"&&IsWindowVisible(h)){f=h;return false;}return true;},IntPtr.Zero);return f;}
public static string BoxText(IntPtr b){var o=new StringBuilder();EnumChildWindows(b,(h,x)=>{if(C(h)=="Static"){var s=new StringBuilder(512);GetWindowText(h,s,512);o.Append(s.ToString());}return true;},IntPtr.Zero);return o.ToString().Replace("\n"," / ");}
public static IntPtr Host(){return FindWindow("FractalBranchHost",null);}
public static IntPtr Lv(){var v=FindWindowEx(FindWindow("Progman",null),IntPtr.Zero,"SHELLDLL_DefView",null);return FindWindowEx(v,IntPtr.Zero,"SysListView32",null);}
public static int Panels(){int n=0;EnumChildWindows(FindWindow("Progman",null),(h,x)=>{if(C(h)=="FractalBranchPanel")n++;return true;},IntPtr.Zero);return n;}
}
'@
$IDOK=1; $IDYES=6; $IDNO=7
$dir="$env:LOCALAPPDATA\Programs\FractalBranch"; $exe="$dir\FractalBranch.exe"
$layout="$env:LOCALAPPDATA\FractalBranch\layout.json"
$arp='HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\FractalBranch'
$run='HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$lnk="$env:APPDATA\Microsoft\Windows\Start Menu\Programs\" + [string]::Join('',[char[]](0x5206,0x5F62,0x6811,0x679D)) + '.lnk'

function Click($proc,$button){ for($i=0;$i -lt 200;$i++){ if($proc.HasExited){ return "(process exited, code $($proc.ExitCode))" }; $b=[T]::Box([uint32]$proc.Id); if($b -ne [IntPtr]::Zero){ $t=[T]::BoxText($b); [void][T]::PostMessage($b,0x111,[IntPtr]$button,[IntPtr]0); Start-Sleep -Milliseconds 400; return $t }; Start-Sleep -Milliseconds 50 }; return '(no dialog)' }
function State($label){
  $l=Get-Item -LiteralPath $lnk -ErrorAction SilentlyContinue
  $runv=(Get-ItemProperty $run -ErrorAction SilentlyContinue).FractalBranch
  $a=Get-ItemProperty $arp -ErrorAction SilentlyContinue
  "[$label] exe=$(Test-Path $exe) autostart=$([bool]$runv) arp=$([bool]$a) shortcut=$([bool]$l) running=$(@(Get-Process FractalBranch -ErrorAction SilentlyContinue).Count) panels=$([T]::Panels()) icons_hidden=$(-not [T]::IsWindowVisible([T]::Lv())) layout=$(Test-Path $layout)"
}
function Wait-Gone($path,$sec){ $d=(Get-Date).AddSeconds($sec); while((Test-Path $path) -and (Get-Date) -lt $d){ Start-Sleep -Milliseconds 200 }; -not (Test-Path $path) }
function Stop-App(){ [void][T]::PostMessage([T]::Host(),0x10,[IntPtr]0,[IntPtr]0); Get-Process FractalBranch -ErrorAction SilentlyContinue | Wait-Process -Timeout 5 -ErrorAction SilentlyContinue }

New-Item -ItemType Directory -Force -Path $Backup | Out-Null
Copy-Item $layout "$Backup\layout.before.json" -Force
State 'start'

"--- cancel at the first dialog changes nothing"
$h0=(Get-FileHash $exe -ErrorAction SilentlyContinue).Hash
$p=Start-Process $Setup -PassThru; "dialog: " + (Click $p $IDNO); $p.WaitForExit(5000)|Out-Null; "exit=$($p.ExitCode) exe_unchanged=$($h0 -eq (Get-FileHash $exe -ErrorAction SilentlyContinue).Hash) running=$(@(Get-Process FractalBranch -ErrorAction SilentlyContinue).Count)"

"--- double click install, over an existing running copy"
$p=Start-Process $Setup -PassThru; "dialog 1: " + (Click $p $IDYES); "dialog 2: " + (Click $p $IDOK); $p.WaitForExit(10000)|Out-Null; "exit=$($p.ExitCode)"
Start-Sleep -Seconds 2
State 'installed'
"exe matches package: " + ((Get-FileHash $exe).Hash -eq (Get-FileHash $Setup).Hash)
"running from: " + ((Get-Process FractalBranch | Select-Object -First 1).Path)
$a=Get-ItemProperty $arp; "arp: $($a.DisplayName) $($a.DisplayVersion) | $($a.UninstallString) | size=$($a.EstimatedSize) KB"
"autostart: " + (Get-ItemProperty $run).FractalBranch
$s=(New-Object -ComObject WScript.Shell).CreateShortcut($lnk); "shortcut -> $($s.TargetPath)"

"--- uninstall through dialogs (keep data)"
$p=Start-Process $exe -ArgumentList '--uninstall' -PassThru; "dialog 1: " + (Click $p $IDYES); "dialog 2: " + (Click $p $IDNO); "dialog 3: " + (Click $p $IDOK); $p.WaitForExit(10000)|Out-Null
"dir removed: " + (Wait-Gone $dir 25)
State 'uninstalled'

"--- first run on a clean profile (no old layout, no XiaoZhi data)"
$fresh="$env:TEMP\fb-fresh"; Remove-Item $fresh -Recurse -Force -ErrorAction SilentlyContinue; New-Item -ItemType Directory -Force -Path $fresh | Out-Null
$saved=$env:LOCALAPPDATA; $env:LOCALAPPDATA=$fresh
$tmpExe="$fresh\FractalBranch.exe"; Copy-Item $Setup $tmpExe
$q=Start-Process $tmpExe -PassThru; Start-Sleep -Seconds 3
"fresh: running=$(-not $q.HasExited) panels=$([T]::Panels()) icons_hidden=$(-not [T]::IsWindowVisible([T]::Lv()))"
Stop-App
$env:LOCALAPPDATA=$saved
"fresh log:"; Get-Content "$fresh\FractalBranch\log.txt" -Encoding UTF8 | Select-Object -Last 3
$j=Get-Content "$fresh\FractalBranch\layout.json" -Raw -Encoding UTF8 | ConvertFrom-Json; "fresh layout: partitions=$($j.partitions.Count) desktop_items=$($j.desktop.items.Count) alpha=$($j.style.bg_alpha)"
"icons restored after exit: " + [T]::IsWindowVisible([T]::Lv())
Remove-Item $fresh -Recurse -Force -ErrorAction SilentlyContinue

"--- quiet reinstall, leaves the app running"
$p=Start-Process $Setup -ArgumentList '--quiet' -PassThru; $p.WaitForExit(15000)|Out-Null; "exit=$($p.ExitCode)"
Start-Sleep -Seconds 3
State 'final'
Start-Sleep -Seconds 2
$before=Get-Content "$Backup\layout.before.json" -Raw -Encoding UTF8 | ConvertFrom-Json
$after=Get-Content $layout -Raw -Encoding UTF8 | ConvertFrom-Json
function Summary($l){ ($l.partitions | ForEach-Object { "$($_.tabs[0].name)=$([math]::Round($_.x)),$([math]::Round($_.y)),$(($_.tabs | ForEach-Object { $_.items.Count } | Measure-Object -Sum).Sum)" }) -join ' ' }
"layout before: desktop=$($before.desktop.items.Count) $(Summary $before)"
"layout after:  desktop=$($after.desktop.items.Count) $(Summary $after)"
