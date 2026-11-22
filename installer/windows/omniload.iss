; Instalador de OmniLoad para Windows (Inno Setup 6).
; Se compila en CI con:  iscc /DAppVersion=0.4.0 installer\windows\omniload.iss
; Se instala solo para el usuario (sin permisos de administrador), así la
; actualización dentro de la app no pide nada.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6F1D6C0E-8B7A-4C57-9E7E-0C2A5B1D4F10}
AppName=OmniLoad
AppVersion={#AppVersion}
AppVerName=OmniLoad {#AppVersion}
AppPublisher=OmniLoad
AppPublisherURL=https://github.com/Danijimzzz/OmniLoad
AppSupportURL=https://github.com/Danijimzzz/OmniLoad/issues
DefaultDirName={autopf}\OmniLoad
DefaultGroupName=OmniLoad
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\dist
OutputBaseFilename=OmniLoad-Setup-{#AppVersion}-windows-x64
SetupIconFile=..\..\apps\omni-gui\ui\icon.ico
UninstallDisplayIcon={app}\omniload.exe
LicenseFile=..\..\LICENSE
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; Para actualizar con la app abierta: cierra OmniLoad y el servicio (omnid).
CloseApplications=yes
CloseApplicationsFilter=omniload.exe,omnid.exe,omni.exe
RestartApplications=no

[Languages]
Name: "spanish"; MessagesFile: "compiler:Languages\Spanish.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\..\target\release\omniload.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\omnid.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\omni.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\OmniLoad"; Filename: "{app}\omniload.exe"
Name: "{autodesktop}\OmniLoad"; Filename: "{app}\omniload.exe"; Tasks: desktopicon

[Run]
; También tras una instalación silenciosa (la actualización dentro de la app
; la usa): así OmniLoad vuelve a abrirse solo.
Filename: "{app}\omniload.exe"; Description: "{cm:LaunchProgram,OmniLoad}"; Flags: nowait postinstall
