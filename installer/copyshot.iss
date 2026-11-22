#define AppName "Copyshot"
#define AppVersion "0.2.0"
#define AppPublisher "Copyshot"
#define AppExeName "copyshot.exe"
; Override with ISCC /DExeDir=... when building into another cargo target dir.
#ifndef ExeDir
  #define ExeDir "..\target\release"
#endif

[Setup]
AppId={{943D869F-DDAA-444D-8459-DDF99205CF11}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=yes
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=Copyshot-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; Copyshot sits in the tray with no visible window, so close it without asking.
CloseApplications=force
SetupLogging=yes
SetupIconFile=..\src\copyshot.ico
UninstallDisplayIcon={app}\{#AppExeName}
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
VersionInfoVersion={#AppVersion}.0
VersionInfoProductName={#AppName}
VersionInfoProductVersion={#AppVersion}

[Files]
Source: "{#ExeDir}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"

[Registry]
; Copyshot writes its own startup entry; only make sure uninstalling removes it.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "Copyshot"; Flags: uninsdeletevalue dontcreatekey

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Start Copyshot"; Flags: nowait postinstall skipifsilent
Filename: "{app}\{#AppExeName}"; Flags: nowait skipifdoesntexist; Check: IsAutoUpdate

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/IM {#AppExeName} /F"; Flags: runhidden; RunOnceId: "StopCopyshot"

[UninstallDelete]
Type: filesandordirs; Name: "{localappdata}\Copyshot"

[Code]
function IsAutoUpdate: Boolean;
begin
  Result := ExpandConstant('{param:AUTOLAUNCH|0}') = '1';
end;
