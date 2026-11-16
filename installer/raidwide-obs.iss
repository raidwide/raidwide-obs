; Windows installer for the Raidwide OBS plugin (Inno Setup 6).
; Built by CI: iscc /DAppVersion=x.y.z installer\raidwide-obs.iss, after cargo build --release.
;
; OBS loads plugins from C:\ProgramData\obs-studio\plugins\<name>\, with a layout depending on its version:
;   OBS 33 or newer: <name>\<name>.dll
;   OBS 31 or 32:    <name>\bin\64bit\<name>.dll
; The installer finds OBS' version and installs to the matching one, removing the plugin from the other
; layouts (and from the legacy Program Files location) so that OBS never loads it twice.

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif

#define PluginName "raidwide_ffxiv_rs"

[Setup]
; Never change the AppId: it ties upgrades and the uninstaller to earlier installs
AppId={{CE2755EB-54A4-4DF3-B584-5DE83022F54A}
AppName=Raidwide OBS plugin
AppVersion={#AppVersion}
AppPublisher=Raidwide
AppPublisherURL=https://github.com/raidwide/raidwide-obs
DefaultDirName={commonappdata}\obs-studio\plugins\{#PluginName}
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayName=Raidwide OBS plugin
OutputDir=..\target\installer
; A fixed name, so that releases/latest/download/raidwide-obs-setup.exe always points to the latest
OutputBaseFilename=raidwide-obs-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Files]
Source: "..\target\release\{#PluginName}.dll"; DestDir: "{code:PluginBinDir}"; Flags: ignoreversion

[UninstallDelete]
Type: dirifempty; Name: "{app}\bin\64bit"
Type: dirifempty; Name: "{app}\bin"
Type: dirifempty; Name: "{app}"

[Code]
var
  ObsDir: String;
  ObsMajor: Integer; // 0 when OBS wasn't found

// OBS' installer records its folder under the 32-bit registry view
function FindObsDir: String;
begin
  if not RegQueryStringValue(HKLM32, 'SOFTWARE\OBS Studio', '', Result) then
    if not RegQueryStringValue(HKLM64, 'SOFTWARE\OBS Studio', '', Result) then
      Result := '';
end;

function FindObsMajor(Dir: String): Integer;
var
  Version: String;
  Dot: Integer;
begin
  Result := 0;
  if (Dir <> '') and GetVersionNumbersString(Dir + '\bin\64bit\obs64.exe', Version) then
  begin
    Dot := Pos('.', Version);
    if Dot > 0 then
      Result := StrToIntDef(Copy(Version, 1, Dot - 1), 0);
  end;
end;

function InitializeSetup: Boolean;
begin
  Result := True;
  ObsDir := FindObsDir;
  ObsMajor := FindObsMajor(ObsDir);
  if ObsMajor = 0 then
    Result := SuppressibleMsgBox(
      'OBS Studio was not found. The plugin needs OBS 31 or newer.' #13#10#13#10
      + 'Install it anyway, for OBS 33 or newer?', mbConfirmation, MB_YESNO, IDYES) = IDYES
  else if ObsMajor < 31 then
  begin
    SuppressibleMsgBox(Format('OBS Studio %d is installed, but the plugin needs OBS 31 or newer. '
      + 'Update OBS, then run this installer again.', [ObsMajor]), mbCriticalError, MB_OK, IDOK);
    Result := False;
  end;
end;

function PluginBinDir(Param: String): String;
begin
  if (ObsMajor = 31) or (ObsMajor = 32) then
    Result := ExpandConstant('{app}\bin\64bit')
  else
    Result := ExpandConstant('{app}');
end;

function IsObsRunning: Boolean;
var
  Locator, Service, Processes: Variant;
begin
  Result := False;
  try
    Locator := CreateOleObject('WbemScripting.SWbemLocator');
    Service := Locator.ConnectServer('.', 'root\CIMV2');
    Processes := Service.ExecQuery('SELECT ProcessId FROM Win32_Process WHERE Name = ''obs64.exe''');
    Result := Processes.Count > 0;
  except
    // Can't tell: let the install go on, a locked DLL is still reported by Setup itself
  end;
end;

// OBS only loads plugins at startup and keeps the DLL locked while running
function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  while IsObsRunning do
    if SuppressibleMsgBox('OBS Studio is running. Close it, then click Retry.',
        mbError, MB_RETRYCANCEL, IDCANCEL) = IDCANCEL then
    begin
      Result := 'OBS Studio is running. Close it and run the installer again.';
      Exit;
    end;
end;

procedure DeleteIfOther(Path: String);
begin
  if (CompareText(ExtractFileDir(Path), PluginBinDir('')) <> 0) and FileExists(Path) then
    DeleteFile(Path);
end;

// Removes the plugin from the layouts OBS isn't using, e.g. a manual copy or an install made
// before OBS was updated, which would otherwise be loaded twice or left stale
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
  begin
    DeleteIfOther(ExpandConstant('{app}\{#PluginName}.dll'));
    DeleteIfOther(ExpandConstant('{app}\bin\64bit\{#PluginName}.dll'));
    if ObsDir <> '' then
      DeleteIfOther(ObsDir + '\obs-plugins\64bit\{#PluginName}.dll');
  end;
end;
