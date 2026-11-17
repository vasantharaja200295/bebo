# Builds bebo and makes two installer packages: dist\bebo-<version>-setup.exe and dist\bebo-<version>-x64.zip.
# The setup .exe is made with IExpress, which is part of Windows.
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

# IExpress extracts the files to a temporary folder and runs install.ps1 with a hidden console.
$setup = Join-Path $root "dist\bebo-$version-setup.exe"
Remove-Item $setup -ErrorAction SilentlyContinue
$sed = Join-Path $root 'target\package\setup.sed'
@"
[Version]
Class=IEXPRESS
SEDVersion=3
[Options]
PackagePurpose=InstallApp
ShowInstallProgramWindow=1
HideExtractAnimation=1
UseLongFileName=1
InsideCompressed=0
CAB_FixedSize=0
CAB_ResvCodeSigning=0
RebootMode=N
InstallPrompt=%InstallPrompt%
DisplayLicense=%DisplayLicense%
FinishMessage=%FinishMessage%
TargetName=%TargetName%
FriendlyName=%FriendlyName%
AppLaunched=%AppLaunched%
PostInstallCmd=%PostInstallCmd%
AdminQuietInstCmd=%AdminQuietInstCmd%
UserQuietInstCmd=%UserQuietInstCmd%
SourceFiles=SourceFiles
[Strings]
InstallPrompt=Install bebo for the current user? Setup puts bebo in your AppData Programs folder, adds it to the Start menu, and adds it to Installed apps. Administrator rights are not necessary.
DisplayLicense=
FinishMessage=
TargetName=$setup
FriendlyName=bebo setup
AppLaunched=powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File install.ps1 -Box
PostInstallCmd=<None>
AdminQuietInstCmd=
UserQuietInstCmd=
FILE0="bebo.exe"
FILE1="install.ps1"
FILE2="uninstall.ps1"
[SourceFiles]
SourceFiles0=$stage\
[SourceFiles0]
%FILE0%=
%FILE1%=
%FILE2%=
"@ | Set-Content $sed -Encoding ASCII
# ponytail: IExpress does not accept quote marks around the SED path, so the project path must not contain spaces.
Start-Process iexpress.exe -ArgumentList '/N', '/Q', $sed -Wait
if (-not (Test-Path $setup)) { Pop-Location; throw 'IExpress did not make the setup .exe' }
& (Join-Path $PSScriptRoot 'set-icon.ps1') -Exe $setup -Ico (Join-Path $root 'assets\app\bebo-dark.ico')
Pop-Location
Write-Host "Package: $setup"
Write-Host "Package: $zip"
