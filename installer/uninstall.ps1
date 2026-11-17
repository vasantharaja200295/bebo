# Removes bebo for the current user: the program, the Start menu shortcut, and the entry in "Installed apps".
# The settings in %APPDATA%\bebo stay. Delete that folder to remove them also.
$dir = Join-Path $env:LOCALAPPDATA 'Programs\bebo'
if (Get-Process bebo -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq (Join-Path $dir 'bebo.exe') }) {
    Write-Host 'Close all bebo windows, then run the uninstaller again.' -ForegroundColor Yellow
    exit 1
}
Remove-Item (Join-Path ([Environment]::GetFolderPath('Programs')) 'bebo.lnk') -ErrorAction SilentlyContinue
Remove-Item 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\bebo' -Recurse -ErrorAction SilentlyContinue
# The script is in the folder that it removes. A second process removes the folder after this script ends.
Start-Process cmd.exe -ArgumentList "/c timeout /t 2 >nul & rmdir /s /q `"$dir`"" -WindowStyle Hidden
Write-Host 'bebo is removed.' -ForegroundColor Green
