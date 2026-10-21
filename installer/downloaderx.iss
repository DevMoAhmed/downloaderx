; DownloaderX installer. Build: cargo build --release, then
;   "%LOCALAPPDATA%\Programs\Inno Setup 6\ISCC.exe" installer\downloaderx.iss
; Output: dist\DownloaderX-Setup-<version>.exe

#define AppName "DownloaderX"
#define AppVersion "0.4.2"
#define AppExe "downloaderx.exe"

[Setup]
AppId={{B6F4C8E2-3D5A-4F7B-9C1E-2A8D6F0E4B13}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppName}
AppPublisherURL=https://github.com/DevMoAhmed/downloaderx
AppSupportURL=https://github.com/DevMoAhmed/downloaderx/issues
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; Per-user install by default (no admin prompt); the wizard offers "all users" too.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
UsedUserAreasWarning=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
OutputBaseFilename={#AppName}-Setup-{#AppVersion}
SetupIconFile=..\assets\downloaderx.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
WizardStyle=modern
WizardImageFile=..\assets\wizard.bmp,..\assets\wizard@2x.bmp
WizardSmallImageFile=..\assets\wizard-small.bmp,..\assets\wizard-small@2x.bmp
Compression=lzma2/max
SolidCompression=yes
CloseApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
WelcomeLabel2=This will install [name/ver] on your computer.%n%nDownloaderX speeds up downloads with multiple connections, saves videos and streams, and works with Chrome and Edge through its browser extension.%n%nIt is recommended that you close other applications before continuing.

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "autostart"; Description: "Start DownloaderX with Windows (keeps the browser extension working)"; GroupDescription: "Startup:"
Name: "tools"; Description: "Install yt-dlp and ffmpeg with winget (YouTube-style sites, .mp4 streams)"; GroupDescription: "Optional tools:"; Check: ToolsMissing

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\extension\*"; DestDir: "{app}\extension"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\assets\fonts\LICENSE.txt"; DestDir: "{app}"; DestName: "FONTS-LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autoprograms}\{#AppName} browser extension folder"; Filename: "{app}\extension"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; Same value the app's Options > "Launch on Windows startup" writes.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"" --hidden"; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "winget.exe"; Parameters: "install --id yt-dlp.yt-dlp -e --silent --accept-source-agreements --accept-package-agreements"; StatusMsg: "Installing yt-dlp..."; Flags: runhidden waituntilterminated; Tasks: tools
Filename: "winget.exe"; Parameters: "install --id Gyan.FFmpeg -e --silent --accept-source-agreements --accept-package-agreements"; StatusMsg: "Installing ffmpeg..."; Flags: runhidden waituntilterminated; Tasks: tools
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
Filename: "{app}\{#AppExe}"; Parameters: "--setup-extension"; Description: "Show me how to add the DownloaderX extension to Chrome or Edge"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im {#AppExe}"; Flags: runhidden; RunOnceId: "StopApp"

[UninstallDelete]
; WebView2 cache only. The download list and settings in %APPDATA%\downloaderx are kept.
Type: filesandordirs; Name: "{localappdata}\{#AppName}\WebView2"
Type: dirifempty; Name: "{localappdata}\{#AppName}"

[Code]
function OnPath(Exe: String): Boolean;
var
  Code: Integer;
begin
  Result := Exec(ExpandConstant('{cmd}'), '/c where ' + Exe, '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0);
end;

var
  ToolsChecked, ToolsAreMissing: Boolean;

// Inno evaluates Check: functions often, so look only once.
function ToolsMissing: Boolean;
begin
  if not ToolsChecked then begin
    ToolsAreMissing := not (OnPath('yt-dlp') and OnPath('ffmpeg'));
    ToolsChecked := True;
  end;
  Result := ToolsAreMissing;
end;

// An older version may be running (often hidden in the tray); stop it so its exe can be replaced.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /im {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(500);
  Result := '';
end;
