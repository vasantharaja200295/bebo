# Builds bebo and makes the setup wizard dist\bebo-<version>-setup.exe with Inno Setup (installer\bebo.iss).
# Inno Setup 6 must be installed on the build PC. The PCs that install bebo need nothing.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$iscc = Get-ChildItem 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe', 'C:\Program Files\Inno Setup 6\ISCC.exe', "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 is not installed. Install it with: winget install JRSoftware.InnoSetup' }
Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    $version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
    & $iscc.FullName /Q "/DAppVersion=$version" installer\bebo.iss
    if ($LASTEXITCODE -ne 0) { throw 'Inno Setup could not make the setup' }
    Write-Host "Setup: $root\dist\bebo-$version-setup.exe"
} finally { Pop-Location }
