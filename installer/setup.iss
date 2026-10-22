; BitGalaxy.GifRecorder 安装包脚本（Inno Setup 6）
; 编译：ISCC.exe setup.iss  → 产物输出到 ..\dist\
; 注意：本文件含中文，必须以 UTF-8 with BOM 保存

#define MyAppName "BitGalaxy GIF 录屏"
#define MyAppNameEn "BitGalaxy.GifRecorder"
#define MyAppVersion "1.3.0"
#define MyAppPublisher "BitGalaxy"
#define MyAppExeName "bitgalaxy_gifrecorder.exe"

[Setup]
AppId={{8B7A2E1F-3C4D-4E5F-9A6B-7C8D9E0F1A2B}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL=https://github.com/BitGalaxy
; 安装包 EXE 图标 + 卸载列表图标
SetupIconFile={#SourcePath}\..\assets\app.ico
UninstallDisplayName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
; 安装目标
DefaultDirName={autopf}\{#MyAppNameEn}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
; 系统要求（manifest 声明 Win10/11）
MinVersion=10.0
ArchitecturesInstallIn64BitMode=x64compatible
; 压缩与输出
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
OutputDir={#SourcePath}\..\dist
OutputBaseFilename={#MyAppNameEn}-Setup-{#MyAppVersion}
; 许可协议（MIT）
LicenseFile={#SourcePath}\..\LICENSE

[Languages]
Name: "chinesesimplified"; MessagesFile: "ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourcePath}\..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent
