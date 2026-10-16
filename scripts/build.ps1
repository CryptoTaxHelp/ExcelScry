# Build ExcelScry and collect the two deliverables:
#
#   dist/ExcelScry-portable/ExcelScry.exe   single-file portable executable
#   dist/ExcelScry-<version>-setup.exe      NSIS installer
#
# Run from anywhere:  powershell -ExecutionPolicy Bypass -File scripts/build.ps1

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $root

$conf = Get-Content "src-tauri/tauri.conf.json" -Raw | ConvertFrom-Json
# The version is read from the manifest, never hardcoded here: a stale literal
# would silently mislabel the installer. scripts/bump_version.py keeps
# tauri.conf.json, Cargo.toml, Cargo.lock and the README in step.
$version = $conf.version
if (-not $version) { throw "could not read the version from src-tauri/tauri.conf.json" }

Write-Host "==> Building ExcelScry $version"
cargo tauri build
if ($LASTEXITCODE -ne 0) { throw "cargo tauri build failed with exit code $LASTEXITCODE" }

$bin = "src-tauri/target/release/ExcelScry.exe"
if (-not (Test-Path $bin)) { throw "expected $bin to exist after the build" }

$setup = Get-ChildItem "src-tauri/target/release/bundle/nsis" -Filter "*-setup.exe" |
    Select-Object -First 1
if (-not $setup) { throw "no NSIS installer was produced" }

Write-Host "==> Collecting artifacts into dist/"
if (Test-Path dist) { Remove-Item dist -Recurse -Force }
New-Item -ItemType Directory -Path "dist/ExcelScry-portable/samples" -Force | Out-Null

Copy-Item $bin "dist/ExcelScry-portable/ExcelScry.exe"
Copy-Item "README.md" "dist/ExcelScry-portable/README.md"
if (Test-Path "samples") {
    Copy-Item "samples/*.xlsx" "dist/ExcelScry-portable/samples/" -ErrorAction SilentlyContinue
    Copy-Item "samples/*.xlsm" "dist/ExcelScry-portable/samples/" -ErrorAction SilentlyContinue
}
$setupTarget = "dist/ExcelScry-$version-setup.exe"
Copy-Item $setup.FullName $setupTarget

Write-Host ""
Write-Host "Artifacts:"
foreach ($path in @("dist/ExcelScry-portable/ExcelScry.exe", $setupTarget)) {
    $size = (Get-Item $path).Length / 1MB
    Write-Host ("  {0,10:N2} MB  {1}" -f $size, $path)
}
Write-Host ""
Write-Host "The installer adds a 'Scry Excel File' context-menu entry and registers"
Write-Host "ExcelScry with Windows' Default Apps page. On Windows 11 the menu entry"
Write-Host "appears under 'Show more options'."
