# Live self test against a temporary data directory. Drives real windows and real popup
# menus with posted messages, so it needs no hands, and never touches the real layout.
# Menus flash briefly near the mouse cursor; Show Desktop runs once for a screenshot.
param([string]$Data, [string]$Shot)
$env:FRACTALBRANCH_DATA = $Data
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;using System.Text;using System.Runtime.InteropServices;using System.Collections.Generic;
public class T{
public delegate bool E(IntPtr h,IntPtr l);
[StructLayout(LayoutKind.Sequential)]public struct R{public int L,T,Rr,B;}
[DllImport("user32",CharSet=CharSet.Unicode)]public static extern IntPtr FindWindow(string c,string t);
[DllImport("user32",CharSet=CharSet.Unicode)]public static extern IntPtr FindWindowEx(IntPtr p,IntPtr a,string c,string t);
[DllImport("user32")]public static extern bool EnumChildWindows(IntPtr p,E e,IntPtr l);
[DllImport("user32")]public static extern bool EnumWindows(E e,IntPtr l);
[DllImport("user32",CharSet=CharSet.Unicode)]public static extern int GetClassName(IntPtr h,StringBuilder s,int n);
[DllImport("user32")]public static extern bool PostMessage(IntPtr h,uint m,IntPtr w,IntPtr l);
[DllImport("user32")]public static extern IntPtr SendMessage(IntPtr h,uint m,IntPtr w,IntPtr l);
[DllImport("user32")]public static extern bool GetWindowRect(IntPtr h,out R r);
[DllImport("user32")]public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32")]public static extern IntPtr SetProcessDpiAwarenessContext(IntPtr c);
[DllImport("user32")]public static extern int GetSystemMetrics(int i);
[DllImport("user32")]public static extern int GetMenuItemCount(IntPtr m);
[DllImport("user32",CharSet=CharSet.Unicode)]public static extern int GetMenuString(IntPtr m,uint i,StringBuilder s,int n,uint f);
[DllImport("user32")]public static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
public static string C(IntPtr h){var c=new StringBuilder(64);GetClassName(h,c,64);return c.ToString();}
public static List<IntPtr> Panels(){var l=new List<IntPtr>();EnumChildWindows(FindWindow("Progman",null),(h,x)=>{if(C(h)=="FractalBranchPanel")l.Add(h);return true;},IntPtr.Zero);return l;}
public static IntPtr Host(){return FindWindow("FractalBranchHost",null);}
public static IntPtr Menu(){IntPtr f=IntPtr.Zero;EnumWindows((h,x)=>{if(C(h)=="#32768"&&IsWindowVisible(h)){f=h;return false;}return true;},IntPtr.Zero);return f;}
public static IntPtr Lv(){var v=FindWindowEx(FindWindow("Progman",null),IntPtr.Zero,"SHELLDLL_DefView",null);return FindWindowEx(v,IntPtr.Zero,"SysListView32",null);}
public static IntPtr Edit(uint pid){IntPtr f=IntPtr.Zero;EnumWindows((h,x)=>{uint p;GetWindowThreadProcessId(h,out p);if(p==pid&&C(h)=="Edit"){f=h;return false;}return true;},IntPtr.Zero);return f;}
public static string Items(IntPtr m){var o=new List<string>();int n=GetMenuItemCount(m);for(uint i=0;i<n;i++){var s=new StringBuilder(128);GetMenuString(m,i,s,128,0x400);o.Add(s.Length==0?"-":s.ToString());}return string.Join(" | ",o);}
public static IntPtr L(int x,int y){return (IntPtr)((y<<16)|(x&0xffff));}
}
'@
[void][T]::SetProcessDpiAwarenessContext([IntPtr](-4))
$WM_RUP=0x205; $WM_KEY=0x100; $VK_DOWN=[IntPtr]0x28; $VK_RET=[IntPtr]0x0D; $VK_ESC=[IntPtr]0x1B

function Find-Panel($x,$y){ foreach($h in [T]::Panels()){ $r=New-Object T+R; [void][T]::GetWindowRect($h,[ref]$r); if($r.L -eq $x -and $r.T -eq $y){ return $h } }; return [IntPtr]::Zero }
function Wait-Menu(){ for($i=0;$i -lt 100;$i++){ $m=[T]::Menu(); if($m -ne [IntPtr]::Zero -and [T]::IsWindowVisible($m)){ return $m }; Start-Sleep -Milliseconds 30 }; return [IntPtr]::Zero }
function Close-Menu($m){ [void][T]::PostMessage($m,$WM_KEY,$VK_ESC,[IntPtr]0); Start-Sleep -Milliseconds 200; if([T]::IsWindowVisible($m)){ [void][T]::PostMessage([T]::Host(),0x1F,[IntPtr]0,[IntPtr]0); Start-Sleep -Milliseconds 200 } }
function Dump($label){ $m=Wait-Menu; if($m -eq [IntPtr]::Zero){ Write-Host "$label : NO MENU"; return [IntPtr]::Zero }; $hm=[T]::SendMessage($m,0x1E1,[IntPtr]0,[IntPtr]0); Write-Host ("$label : " + [T]::Items($hm)); return $m }
function Pick($m,$index,$main){ [void][T]::SendMessage($m,0x1E5,[IntPtr]$index,[IntPtr]0); Start-Sleep -Milliseconds 80; [void][T]::PostMessage($m,$WM_KEY,$VK_RET,[IntPtr]0); Start-Sleep -Milliseconds 500; $e=[T]::Edit($main); if($e -ne [IntPtr]::Zero){ "  rename box opened, committing"; [void][T]::PostMessage($e,$WM_KEY,$VK_RET,[IntPtr]0) } else { "  NO rename box" }; Start-Sleep -Milliseconds 400 }
function Count-Parts($ps){ $n=0; foreach($p in $ps){ $n++; foreach($t in $p.tabs){ if($t.children){ $n+=Count-Parts $t.children } } }; $n }
function Read-Layout(){ Start-Sleep -Seconds 2; Get-Content "$Data\layout.json" -Raw -Encoding UTF8 | ConvertFrom-Json }

$before=Count-Parts (Get-Content "$Data\layout.json" -Raw -Encoding UTF8 | ConvertFrom-Json).partitions
$p=Start-Process 'C:\Users\vool\FractalBranch\target\release\FractalBranch.exe' -PassThru
Start-Sleep -Seconds 3
"panels on desktop (incl nested): " + [T]::Panels().Count + " ; partitions in layout: $before ; icons hidden: " + (-not [T]::IsWindowVisible([T]::Lv()))
$main=(Get-Process FractalBranch | Sort-Object PrivateMemorySize64 -Descending | Select-Object -First 1).Id
$desk=Find-Panel 0 0
$l0=Get-Content "$Data\layout.json" -Raw -Encoding UTF8 | ConvertFrom-Json
$c0=$l0.partitions | Where-Object { $_.tabs[0].name -eq 'CP simulation' }
$cp=Find-Panel ([int][math]::Round($c0.x*2)) ([int][math]::Round($c0.y*2))
"cp panel handle: $cp"

"--- right click blank desktop"
[void][T]::PostMessage($desk,$WM_RUP,[IntPtr]0,[T]::L(2400,2000)); $m=Dump 'menu'
if($m -ne [IntPtr]::Zero){ Pick $m 0 $main }
$n=Count-Parts (Read-Layout).partitions; "  partitions: $before -> $n"

"--- right click partition title, pick new sub-partition"
[void][T]::PostMessage($cp,$WM_RUP,[IntPtr]0,[T]::L(300,40)); $m=Dump 'menu'
if($m -ne [IntPtr]::Zero){ Pick $m 3 $main }
$n2=Count-Parts (Read-Layout).partitions; "  partitions: $n -> $n2 ; panels now: " + [T]::Panels().Count

"--- right click a desktop icon (system menu)"
[void][T]::PostMessage($desk,$WM_RUP,[IntPtr]0,[T]::L(72,60)); $m=Dump 'menu'
if($m -ne [IntPtr]::Zero){ Close-Menu $m }

"--- tray menu"
[void][T]::PostMessage([T]::Host(),0x8003,[IntPtr]1,[IntPtr]0x205); $m=Dump 'menu'
if($m -ne [IntPtr]::Zero){ Close-Menu $m }

"--- double click blank desktop hides, again restores"
$top=$cp
[void][T]::PostMessage($desk,0x203,[IntPtr]1,[T]::L(2400,2000)); Start-Sleep -Milliseconds 400; "  partition visible after first: " + [T]::IsWindowVisible($top)
[void][T]::PostMessage($desk,0x203,[IntPtr]1,[T]::L(2400,2000)); Start-Sleep -Milliseconds 400; "  partition visible after second: " + [T]::IsWindowVisible($top)

"--- wheel in partition with sub-partitions"
[void][T]::SendMessage($cp,0x20A,[IntPtr]((-240 -shl 16) -band 0xffff0000),[T]::L(3000,100)); Start-Sleep -Milliseconds 300

"--- screenshot"
$shell=New-Object -ComObject Shell.Application; $shell.ToggleDesktop()
try { Start-Sleep -Milliseconds 800; $sw=[T]::GetSystemMetrics(0);$sh=[T]::GetSystemMetrics(1); $b=New-Object Drawing.Bitmap $sw,$sh; $g=[Drawing.Graphics]::FromImage($b); $g.CopyFromScreen(0,0,0,0,$b.Size); $g.Dispose(); $b.Save($Shot); $b.Dispose() } finally { $shell.ToggleDesktop() }

Start-Sleep -Seconds 4
"memory: " + (@(Get-Process FractalBranch | ForEach-Object { "$($_.Id) private=$([math]::Round($_.PrivateMemorySize64/1MB,1)) ws=$([math]::Round($_.WorkingSet64/1MB,1))" }) -join ' ; ')
[void][T]::PostMessage([T]::Host(),0x10,[IntPtr]0,[IntPtr]0)
Get-Process FractalBranch -ErrorAction SilentlyContinue | Wait-Process -Timeout 5 -ErrorAction SilentlyContinue
"exited, procs left: " + @(Get-Process FractalBranch -ErrorAction SilentlyContinue).Count + " ; icons restored: " + [T]::IsWindowVisible([T]::Lv())
