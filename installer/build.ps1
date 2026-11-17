# Builds bebo and makes the installer package dist\bebo-<version>-x64.zip.
# The package has bebo.exe, the install and uninstall scripts, and "Install bebo.cmd".
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
Push-Location $root
cargo build --release
if ($LASTEXITCODE -ne 0) { Pop-Location; throw 'cargo build failed' }
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$stage = Join-Path $root "target\package\bebo-$version"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item target\release\bebo.exe, installer\install.ps1, installer\uninstall.ps1, 'installer\Install bebo.cmd' $stage
New-Item -ItemType Directory -Force dist | Out-Null
$zip = Join-Path $root "dist\bebo-$version-x64.zip"
Remove-Item $zip -ErrorAction SilentlyContinue
Compress-Archive -Path "$stage\*" -DestinationPath $zip
Pop-Location
Write-Host "Package: $zip"
