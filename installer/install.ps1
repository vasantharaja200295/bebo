# Installs bebo for the current user. Administrator rights are not necessary.
# The script copies bebo.exe to %LOCALAPPDATA%\Programs\bebo, adds a Start menu shortcut,
# and adds bebo to "Installed apps" in Windows Settings, so that Windows can uninstall it.
# -Box shows the result in a message box. The setup .exe uses it, because its console is hidden.
param([switch]$Box)
$ErrorActionPreference = 'Stop'
# Returns the button that the user clicked (6 = Yes). Without -Box, the text goes to the console.
function Say([string]$text, [int]$type) {
    if ($Box) { (New-Object -ComObject WScript.Shell).Popup($text, 0, 'bebo setup', $type) } else { Write-Host $text }
}
# The console of the setup .exe is hidden, so an error must show in a message box.
trap { Say "bebo could not be installed.`n`n$_" 16 | Out-Null; exit 1 }
$source = Join-Path $PSScriptRoot 'bebo.exe'
$dir = Join-Path $env:LOCALAPPDATA 'Programs\bebo'
$exe = Join-Path $dir 'bebo.exe'

if (Get-Process bebo -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }) {
    Say 'Close all bebo windows, then run the installer again.' 48 | Out-Null
    exit 1
}

New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item $source $exe -Force
Copy-Item (Join-Path $PSScriptRoot 'uninstall.ps1') (Join-Path $dir 'uninstall.ps1') -Force

$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut((Join-Path ([Environment]::GetFolderPath('Programs')) 'bebo.lnk'))
$link.TargetPath = $exe
$link.WorkingDirectory = $env:USERPROFILE
$link.IconLocation = "$exe,0"
$link.Description = 'bebo terminal'
$link.Save()

$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\bebo'
New-Item -Path $key -Force | Out-Null
$values = @{
    DisplayName = 'bebo'
    DisplayVersion = (Get-Item $exe).VersionInfo.ProductVersion
    Publisher = 'bebo'
    DisplayIcon = "$exe,0"
    InstallLocation = $dir
    UninstallString = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$dir\uninstall.ps1`""
}
foreach ($name in $values.Keys) { Set-ItemProperty -Path $key -Name $name -Value $values[$name] }
Set-ItemProperty -Path $key -Name NoModify -Value 1 -Type DWord
Set-ItemProperty -Path $key -Name NoRepair -Value 1 -Type DWord
Set-ItemProperty -Path $key -Name EstimatedSize -Value ([int]((Get-Item $exe).Length / 1KB)) -Type DWord

$text = "bebo is installed.`n`nOpen it from the Start menu: type bebo.`nTo remove it, use Settings > Apps > Installed apps."
if (-not $Box) { Say $text 0 | Out-Null; exit }
# 68 = Yes and No buttons with the information icon.
if ((Say "$text`n`nOpen bebo now?" 68) -eq 6) { Start-Process $exe -WorkingDirectory $env:USERPROFILE }
