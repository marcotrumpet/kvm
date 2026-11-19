; Windows installer (Inno Setup 6), built by .github/workflows/release.yml:
;   iscc /DVersion=0.17.0 /DSource=<folder with kvm.exe and WinSparkle.dll> desktop\windows\kvm.iss
; Per user, no admin rights. WinSparkle runs it with /SILENT for updates: it then closes
; the running KVM, replaces it and starts it again.

#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
AppId={{B7E3F0C2-5A1D-4C8E-9F26-3D4B8A7E1C55}
AppName=KVM
AppVersion={#Version}
AppPublisher=KVM
AppPublisherURL=https://github.com/marcotrumpet/kvm
DefaultDirName={localappdata}\Programs\KVM
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputBaseFilename=KVM-windows-setup
OutputDir=..\..\dist
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=force
RestartApplications=no
UninstallDisplayIcon={app}\kvm.exe
WizardStyle=modern

[Files]
Source: "{#Source}\kvm.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\WinSparkle.dll"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\KVM"; Filename: "{app}\kvm.exe"
Name: "{userstartup}\KVM"; Filename: "{app}\kvm.exe"; Tasks: startup

[Tasks]
Name: "startup"; Description: "Start KVM when I sign in"

[Run]
; After an interactive install, offer to open it; after a silent update, just reopen it.
Filename: "{app}\kvm.exe"; Description: "Open KVM"; Flags: nowait postinstall skipifsilent
Filename: "{app}\kvm.exe"; Flags: nowait; Check: WizardSilent

[Messages]
FinishedLabel=KVM is installed and lives in the notification area (the tray). If Windows asks whether KVM may use the network, allow it on private networks, so it can find your Mac directly.

[UninstallRun]
Filename: "taskkill"; Parameters: "/IM kvm.exe /F"; Flags: runhidden; RunOnceId: "StopKVM"
