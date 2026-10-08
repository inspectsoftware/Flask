; Inno Setup script for Flask, the SysCentral task manager.
; Build with build-installer.ps1, which supplies the defines below.

#ifndef AppVersion
  #error AppVersion is not defined. Run build-installer.ps1 instead of compiling this file directly.
#endif
#ifndef SourceExe
  #error SourceExe is not defined. Run build-installer.ps1 instead of compiling this file directly.
#endif
#ifndef OutputName
  #error OutputName is not defined. Run build-installer.ps1 instead of compiling this file directly.
#endif

#define AppName "Flask"
#define AppExe "flask.exe"

[Setup]
; Identifies the product across versions so upgrades replace the old install. Never change it.
AppId={{9872540F-5CD7-423A-9174-B4AD281617AE}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=SysCentral
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\SysCentral\Flask
DisableProgramGroupPage=yes
SetupIconFile=..\assets\flask.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
OutputBaseFilename={#OutputName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; Per-user by default (no UAC prompt); the user can pick "all users" in the dialog.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
; Same name as the app's single-instance mutex: setup and uninstall ask to close a running copy.
AppMutex=SysCentral.Flask.SingleInstance

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

; Offered only to an all-users install: the key below is machine-wide and needs administrator rights.
Name: "replacetaskmgr"; Description: "Open {#AppName} instead of Task Manager (Ctrl+Shift+Esc)"; GroupDescription: "System integration:"; Flags: unchecked; Check: IsAdminInstallMode

[Registry]
; Windows starts the "debugger" named here in place of taskmgr.exe. Uninstalling removes the value and
; Task Manager comes back. Flask's own Startup tab lists this under Image hijacks, as it should.
Root: HKLM; Subkey: "SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe"; ValueType: string; ValueName: "Debugger"; ValueData: """{app}\{#AppExe}"""; Flags: uninsdeletevalue; Tasks: replacetaskmgr

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent shellexec
; shellexec: Flask requires administrator, and only a shell launch can raise the UAC prompt from an unelevated setup.
