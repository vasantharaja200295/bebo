# Installs bebo for the current user. Administrator rights are not necessary.
# The script copies bebo.exe to %LOCALAPPDATA%\Programs\bebo, adds a Start menu shortcut,
# and adds bebo to "Installed apps" in Windows Settings, so that Windows can uninstall it.
$ErrorActionPreference = 'Stop'
$source = Join-Path $PSScriptRoot 'bebo.exe'
$dir = Join-Path $env:LOCALAPPDATA 'Programs\bebo'
$exe = Join-Path $dir 'bebo.exe'

if (Get-Process bebo -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }) {
    Write-Host 'Close all bebo windows, then run the installer again.' -ForegroundColor Yellow
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

Write-Host "bebo is installed in $dir." -ForegroundColor Green
Write-Host 'Open it from the Start menu. To remove it, use Settings > Apps > Installed apps.'
