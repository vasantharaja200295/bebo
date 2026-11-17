; Inno Setup script for the bebo setup wizard. build.ps1 compiles it with ISCC.exe.
; The wizard installs bebo for the current user (no administrator rights) and adds an uninstaller
; to Settings > Apps > Installed apps. The user can select "all users" in the wizard; that needs administrator rights.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
; The AppId identifies bebo for upgrades and the uninstaller. Do not change it.
AppId={{11380DD5-675F-4EEF-9CC2-BF11AA1EA608}
AppName=bebo
AppVersion={#AppVersion}
AppVerName=bebo {#AppVersion}
AppPublisher=bebo
DefaultDirName={autopf}\bebo
DefaultGroupName=bebo
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
SetupIconFile=..\assets\app\bebo-dark.ico
UninstallDisplayIcon={app}\bebo.exe
UninstallDisplayName=bebo
OutputDir=..\dist
OutputBaseFilename=bebo-{#AppVersion}-setup
Compression=lzma2
SolidCompression=yes
; The wizard asks the user to close bebo if it is open, because Windows cannot replace a program file that is in use.
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\bebo.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\bebo"; Filename: "{app}\bebo.exe"; WorkingDir: "{%USERPROFILE}"
Name: "{autodesktop}\bebo"; Filename: "{app}\bebo.exe"; WorkingDir: "{%USERPROFILE}"; Tasks: desktopicon

[Run]
Filename: "{app}\bebo.exe"; Description: "{cm:LaunchProgram,bebo}"; WorkingDir: "{%USERPROFILE}"; Flags: nowait postinstall skipifsilent
