; Remote Control — Windows host installer.
;
; Design goals (matching the app's own non-negotiables):
;   * No admin required. Installs per-user under %LOCALAPPDATA%\Programs, the
;     same privilege level the app already runs at.
;   * Installs the tray host only. "Start with Windows" is persisted by the
;     app itself (HKCU\...\Run) when launched with --enable-autostart — the
;     installer never writes autostart itself, so there's exactly one place
;     that decides it. It's ticked by default: a remote-control host is only
;     useful while it's running.
;   * Detects the running app via its single-instance mutex and offers to
;     close it before overwriting files, so an upgrade over a running host
;     doesn't fail or leave a half-replaced binary.
;   * Never touches %APPDATA%\RemoteControl (config) or
;     %LOCALAPPDATA%\RemoteControl (identity, pairing, logs) — uninstalling
;     removes the program, not the PC ID or settings.
;
; Build with Inno Setup 6 from the repo root, after
; `cargo build --release -p rc-host -p rc-desktop-client`:
;
;   iscc /DMyAppVersion=0.2.0 /DMyAppURL=https://github.com/SantaClauseMacros/remote-control installer\RemoteControl.iss
;
; Output lands in installer\output\RemoteControlSetup.exe — deliberately
; unversioned, so a release's "latest/download/RemoteControlSetup.exe" link
; always points at the newest installer.

#define MyAppName "Remote Control"
#ifndef MyAppVersion
  #define MyAppVersion "0.2.0"
#endif
#ifndef MyAppURL
  #define MyAppURL "https://github.com"
#endif
#define MyAppPublisher "Remote Control"
#define MyAppExeName "rc-host.exe"
#define MyAppMutex "Local\RemoteControlHost.SingleInstance"

[Setup]
AppId={{B27B6E1E-7B9B-4A2E-9B7B-6B6B0B0B0B01}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}/releases
; Per-user install under %LOCALAPPDATA%\Programs — never Program Files, so no
; elevation prompt ever appears (matches the app's own AppPaths convention).
DefaultDirName={localappdata}\Programs\{#MyAppName}
PrivilegesRequired=lowest
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
OutputDir=output
OutputBaseFilename=RemoteControlSetup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
SetupIconFile=..\host\assets\icon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName}
VersionInfoVersion={#MyAppVersion}
AppMutex={#MyAppMutex}
CloseApplications=yes
RestartApplications=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "autostart"; Description: "Start {#MyAppName} automatically when I sign in (needed to connect while you're away)"; GroupDescription: "Startup:"
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "..\target\release\rc-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\rc-desktop-client.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; Same flag the host itself checks (platform::autostart::AUTOSTART_ARG) so a
; launch here behaves identically to one from the HKCU Run entry: quiet start
; with no window flash. With the "autostart" task, --enable-autostart makes
; the app write its own Run entry, keeping "one place decides autostart".
Filename: "{app}\{#MyAppExeName}"; Parameters: "--autostart"; Flags: nowait postinstall skipifsilent runasoriginaluser; Description: "Launch {#MyAppName} now"; Check: not IsTaskSelected('autostart')
Filename: "{app}\{#MyAppExeName}"; Parameters: "--autostart --enable-autostart"; Flags: nowait postinstall skipifsilent runasoriginaluser; Description: "Launch {#MyAppName} now"; Check: IsTaskSelected('autostart')
Filename: "{#MyAppURL}#set-it-up-in-3-minutes"; Flags: postinstall shellexec skipifsilent unchecked; Description: "Open the setup guide (how to connect from your phone)"

[Code]
function IsTaskSelected(const TaskName: String): Boolean;
begin
  Result := WizardIsTaskSelected(TaskName);
end;
