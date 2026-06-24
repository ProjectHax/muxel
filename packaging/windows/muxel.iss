; Inno Setup script for muxel — a basic, per-user Windows installer.
;
; Installs muxel.exe to %LOCALAPPDATA%\Programs\muxel with a Start Menu shortcut
; and an Add/Remove Programs uninstaller. It is deliberately a PER-USER install
; (PrivilegesRequired=lowest, no admin): the install directory is user-writable,
; so muxel's in-app auto-updater (which self-replaces muxel.exe from the latest
; GitHub Release) keeps working without ever prompting for elevation.
;
; Built in CI by .github/workflows/release.yml, which passes the version, arch,
; and source/asset directories on the command line:
;
;   ISCC /DMyAppVersion=0.0.1 /DMyArch=x86_64 ^
;        /DSourceDir=dist /DAssetDir=crates\muxel\assets ^
;        packaging\windows\muxel.iss
;
; Output: muxel-windows-<arch>-setup.exe (Authenticode-signed post-release by
; scripts/sign-windows.sh).

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif
#ifndef MyArch
  #define MyArch "x86_64"
#endif
#ifndef SourceDir
  #define SourceDir "dist"
#endif
#ifndef AssetDir
  #define AssetDir "crates\muxel\assets"
#endif

#define MyAppName "muxel"
#define MyAppPublisher "ProjectHax LLC"
#define MyAppURL "https://muxel.sh"
#define MyAppExeName "muxel.exe"

[Setup]
; A stable AppId keeps upgrades/uninstall coherent across versions — never change it.
AppId={{F98BF3A2-B63F-42D4-AFD0-8FB927136A54}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={localappdata}\Programs\muxel
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=.
OutputBaseFilename=muxel-windows-{#MyArch}-setup
SetupIconFile={#AssetDir}\muxel.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; Offer to close a running muxel so an over-the-top reinstall/upgrade succeeds.
CloseApplications=yes
#if MyArch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional icons:"; Flags: unchecked

[Files]
Source: "{#SourceDir}\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\muxel"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\muxel"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Launch muxel"; Flags: nowait postinstall skipifsilent
