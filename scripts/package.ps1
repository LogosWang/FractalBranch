# Build, test and package the one-file installer into dist\. Run from anywhere.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $root
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

# Native tools write progress to stderr; only exit codes decide success here.
$ErrorActionPreference = 'Continue'
cmd /c "cargo test -p fractal-branch 2>nul" | Select-String 'test result'
if ($LASTEXITCODE -ne 0) { throw 'tests failed' }
cmd /c "cargo build --release -p fractal-branch 2>nul"
if ($LASTEXITCODE -ne 0) { throw 'build failed' }
$ErrorActionPreference = 'Stop'

$version = (Select-String -Path 'app\Cargo.toml' -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
New-Item -ItemType Directory -Force -Path 'dist' | Out-Null
$out = "dist\FractalBranch-Setup-$version.exe"
Copy-Item 'target\release\FractalBranch.exe' $out -Force
$item = Get-Item $out
"packaged: $($item.FullName)"
"size: $([math]::Round($item.Length / 1KB)) KB"
"sha256: $((Get-FileHash $out -Algorithm SHA256).Hash)"
