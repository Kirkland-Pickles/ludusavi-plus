#ifndef Binary
  #error Binary is required
#endif
#ifndef Version
  #error Version is required
#endif
#ifndef Architecture
  #define Architecture "x64"
#endif
#ifndef Output
  #define Output "..\dist"
#endif

[Setup]
AppId={{78A48778-C44A-45EB-AED3-62D625E21970}
AppName=Ludusavi
AppVersion={#Version}
DefaultDirName={localappdata}\Programs\Ludusavi
DefaultGroupName=Ludusavi
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\ludusavi.exe
OutputDir={#Output}
OutputBaseFilename=ludusavi-{#Version}-{#Architecture}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
#if Architecture == "x64"
ArchitecturesAllowed=x64compatible
#endif

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#Binary}"; DestDir: "{app}"; DestName: "ludusavi.exe"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Ludusavi"; Filename: "{app}\ludusavi.exe"
Name: "{autodesktop}\Ludusavi"; Filename: "{app}\ludusavi.exe"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\ludusavi.exe"; ValueType: string; ValueData: "{app}\ludusavi.exe"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "Ludusavi"; Flags: dontcreatekey uninsdeletevalue

[Run]
Filename: "{app}\ludusavi.exe"; Description: "Open Ludusavi"; Flags: nowait postinstall skipifsilent
