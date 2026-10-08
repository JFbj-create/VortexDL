; ==========================================================================
;  VortexDL 安装包 NSIS 脚本
;  版本: 1.0
;  生成: VortexDL-Setup.exe
; ==========================================================================

!define APP_NAME "VortexDL"
!define APP_FULL_NAME "漩涡下载器 VortexDL"
!define APP_VERSION "1.0"
!define APP_PUBLISHER "VortexDL"
!define APP_EXE "VortexDL.exe"
!define APP_URL "https://vortexdl.app"
!define APP_UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\VortexDL"

; ---------- 编译选项 ----------
Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma
SetCompressorDictSize 32
RequestExecutionLevel admin

; ---------- 引入现代界面 ----------
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "WinVer.nsh"

; ---------- MUI 配置 ----------
!define MUI_ICON "F:\vdgame\VortexDL\src-tauri\icons\icon.ico"
!define MUI_UNICON "F:\vdgame\VortexDL\src-tauri\icons\icon.ico"
!define MUI_ABORTWARNING
!define MUI_ABORTWARNING_TEXT "您确定要退出 VortexDL 安装程序吗?"

; 安装页面
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "F:\vdgame\VortexDL\LICENSE.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

; 卸载页面
!insertmacro MUI_UNPAGE_WELCOME
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH

; ---------- 语言 ----------
!insertmacro MUI_LANGUAGE "SimpChinese"

; ---------- 安装信息 ----------
Name "${APP_FULL_NAME}"
OutFile "F:\vdgame\VortexDL-Setup.exe"
InstallDir "$PROGRAMFILES64\VortexDL"
InstallDirRegKey HKLM "${APP_UNINSTALL_KEY}" "InstallLocation"
ShowInstDetails show
ShowUnInstDetails show
; 左下角品牌文本 (显示版本号)
BrandingText "1.0"

; ---------- 版本信息 (在 exe 属性中显示) ----------
VIProductVersion "1.0.0.0"
VIAddVersionKey "ProductName" "${APP_FULL_NAME}"
VIAddVersionKey "CompanyName" "${APP_PUBLISHER}"
VIAddVersionKey "FileDescription" "${APP_FULL_NAME} 安装程序"
VIAddVersionKey "LegalCopyright" "Copyright (C) 2026 ${APP_PUBLISHER}"
VIAddVersionKey "FileVersion" "${APP_VERSION}"
VIAddVersionKey "ProductVersion" "${APP_VERSION}"

; ==========================================================================
; Section: 主程序
; ==========================================================================
Section "主程序 (必需)" SecMain
  SectionIn RO ; 必选

  SetOutPath "$INSTDIR"
  SetOverwrite on

  ; ----- 主程序 exe (MSVC 编译产物, 直接输出为 VortexDL.exe) -----
  ; 先删除可能残留的旧文件, 避免文件占用导致写入失败
  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\vortex-dl.exe"
  File /oname=VortexDL.exe "F:\vdgame\VortexDL\src-tauri\target\x86_64-pc-windows-msvc\release\vortex-dl.exe"

  ; ----- 资源文件 (来自 resources_main) -----
  File "F:\vdgame\VortexDL\src-tauri\resources_main\swiftfetch.exe"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\WebView2Loader.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\game_index.json"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\translations.json"

  ; ----- MSVC 运行时 DLL (vortex-dl.exe 依赖) -----
  File "F:\vdgame\VortexDL\src-tauri\resources_main\vcruntime140.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\vcruntime140_1.dll"

  ; ----- GCC 运行时 DLL (保留全部, 兼容性备份) -----
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libasprintf-0.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libatomic-1.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libcharset-1.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libgcc_s_seh-1.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libgmp-10.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libgmpxx-4.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libgomp-1.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libiconv-2.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libintl-8.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libisl-23.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libmpc-3.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libmpfr-6.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libquadmath-0.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libstdc++-6.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libwinpthread-1.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\libzstd.dll"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\zlib1.dll"

  ; ----- 前端文件 (src/) -----
  File "F:\vdgame\VortexDL\src\app.js"
  File "F:\vdgame\VortexDL\src\styles.css"
  ; 补全：播放窗口 + 超分管线 + hls 依赖 + 着色器（便携模式缺一个就打不开播放器）
  File "F:\vdgame\VortexDL\src\player.html"
  File "F:\vdgame\VortexDL\src\player.js"
  File "F:\vdgame\VortexDL\src\anime4k.js"
  SetOutPath "$INSTDIR\vendor"
  File /nonfatal "F:\vdgame\VortexDL\src\vendor\hls.min.js"
  SetOutPath "$INSTDIR\shaders"
  File /nonfatal /r "F:\vdgame\VortexDL\src\shaders\*.*"
  SetOutPath "$INSTDIR"
  File "F:\vdgame\VortexDL\src\index.html"
  ; ★ GX 成人库索引（7MB）：带上它，装完第一眼成人页就有 GX 段；
  ;   应用启动时若发现索引为空还会后台自动同步一次（main.rs 的 GX_AUTO_SYNC）。
  SetOutPath "$INSTDIR\data\gx"
  File /nonfatal "F:\vdgame\VortexDL\data\gx\index.json"
  SetOutPath "$INSTDIR"

  ; ----- 前端图标 (icons/) -----
  SetOutPath "$INSTDIR\icons"
  File /r "F:\vdgame\VortexDL\src\icons\*.*"
  SetOutPath "$INSTDIR"

  ; ----- 系统修复工具 (tools/) -----
  SetOutPath "$INSTDIR\tools\c++"
  File "F:\2种模块\系统修复工具\tools\c++\*.*"
  SetOutPath "$INSTDIR\tools\dx"
  File "F:\2种模块\系统修复工具\tools\dx\*.*"
  SetOutPath "$INSTDIR"

  ; ----- 7z 工具 (resources\7z\) -----
  SetOutPath "$INSTDIR\resources\7z"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\7z\7z.exe"
  File "F:\vdgame\VortexDL\src-tauri\resources_main\7z\7z.dll"

  ; ----- 模块教程 -----
  SetOutPath "$INSTDIR\module_tutorial"
  File /r "F:\vdgame\VortexDL\src-tauri\resources_main\module_tutorial\*.*"

  ; ----- 已安装模块 (可选) -----
  SetOutPath "$INSTDIR\modules"
  File /nonfatal /r "F:\vdgame\VortexDL\src-tauri\resources_main\modules\*.*"

  ; ----- 写入注册表 (卸载信息) -----
  WriteRegStr HKLM "${APP_UNINSTALL_KEY}" "DisplayName" "${APP_FULL_NAME}"
  WriteRegStr HKLM "${APP_UNINSTALL_KEY}" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKLM "${APP_UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKLM "${APP_UNINSTALL_KEY}" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr HKLM "${APP_UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${APP_UNINSTALL_KEY}" "URLInfoAbout" "${APP_URL}"
  WriteRegStr HKLM "${APP_UNINSTALL_KEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegDWORD HKLM "${APP_UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${APP_UNINSTALL_KEY}" "NoRepair" 1

  ; ----- 估算占用空间 -----
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKLM "${APP_UNINSTALL_KEY}" "EstimatedSize" $0

  ; ----- 创建卸载程序 -----
  WriteUninstaller "$INSTDIR\uninstall.exe"

SectionEnd

; ==========================================================================
; Section: 快捷方式 (可选)
; ==========================================================================
Section "桌面快捷方式" SecDesktop
  SetOutPath "$INSTDIR"
  CreateShortcut "$DESKTOP\VortexDL.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_EXE}" 0
SectionEnd

Section "开始菜单快捷方式" SecStartMenu
  SetOutPath "$INSTDIR"
  CreateDirectory "$SMPROGRAMS\VortexDL"
  CreateShortcut "$SMPROGRAMS\VortexDL\VortexDL.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_EXE}" 0
  CreateShortcut "$SMPROGRAMS\VortexDL\卸载 VortexDL.lnk" "$INSTDIR\uninstall.exe" "" "$INSTDIR\uninstall.exe" 0
SectionEnd

; ==========================================================================
; Section 描述
; ==========================================================================
!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} "安装 VortexDL 主程序及其运行所需的所有组件 (必需)"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "在桌面创建 VortexDL 启动图标"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecStartMenu} "在开始菜单创建 VortexDL 程序组"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ==========================================================================
; 安装初始化
; ==========================================================================
Function .onInit
  ; ★ 静默杀掉可能正在运行的 VortexDL 进程 (避免文件占用导致写入失败)
  nsExec::ExecToLog 'taskkill /F /IM "${APP_EXE}"'
  nsExec::ExecToLog 'taskkill /F /IM "vortex-dl.exe"'
  nsExec::ExecToLog 'taskkill /F /IM "swiftfetch.exe"'
  Sleep 1000

  ; 检查是否已安装旧版本
  ReadRegStr $0 HKLM "${APP_UNINSTALL_KEY}" "InstallLocation"
  StrCmp $0 "" init_done 0
    MessageBox MB_YESNO|MB_ICONQUESTION "检测到系统已安装 VortexDL。$\n$\n是否先卸载旧版本再继续安装?" IDNO init_done
    ${If} ${FileExists} "$0\uninstall.exe"
      ExecWait '"$0\uninstall.exe" /S _?=$0'
      Sleep 2000
    ${EndIf}
  init_done:
FunctionEnd

Function .onInstSuccess
  ; 安装成功后询问是否立即启动
  MessageBox MB_YESNO|MB_ICONQUESTION "VortexDL 已成功安装!$\n$\n是否立即启动?" IDNO no_launch
    Exec '"$INSTDIR\${APP_EXE}"'
  no_launch:
FunctionEnd

; ==========================================================================
; 卸载
; ==========================================================================
Section "Uninstall"
  ; 删除主程序
  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\uninstall.exe"

  ; 删除资源文件
  Delete "$INSTDIR\swiftfetch.exe"
  Delete "$INSTDIR\WebView2Loader.dll"
  Delete "$INSTDIR\game_index.json"
  Delete "$INSTDIR\translations.json"

  ; 删除 MSVC 运行时 DLL
  Delete "$INSTDIR\vcruntime140.dll"
  Delete "$INSTDIR\vcruntime140_1.dll"
  ; 删除 GCC 运行时 DLL
  Delete "$INSTDIR\libasprintf-0.dll"
  Delete "$INSTDIR\libatomic-1.dll"
  Delete "$INSTDIR\libcharset-1.dll"
  Delete "$INSTDIR\libgcc_s_seh-1.dll"
  Delete "$INSTDIR\libgmp-10.dll"
  Delete "$INSTDIR\libgmpxx-4.dll"
  Delete "$INSTDIR\libgomp-1.dll"
  Delete "$INSTDIR\libiconv-2.dll"
  Delete "$INSTDIR\libintl-8.dll"
  Delete "$INSTDIR\libisl-23.dll"
  Delete "$INSTDIR\libmpc-3.dll"
  Delete "$INSTDIR\libmpfr-6.dll"
  Delete "$INSTDIR\libquadmath-0.dll"
  Delete "$INSTDIR\libstdc++-6.dll"
  Delete "$INSTDIR\libwinpthread-1.dll"
  Delete "$INSTDIR\libzstd.dll"
  Delete "$INSTDIR\zlib1.dll"

  ; 删除前端文件
  Delete "$INSTDIR\app.js"
  Delete "$INSTDIR\styles.css"
  Delete "$INSTDIR\index.html"
  RMDir /r "$INSTDIR\icons"

  ; 删除系统修复工具
  RMDir /r "$INSTDIR\tools"

  ; 删除 7z 工具
  Delete "$INSTDIR\resources\7z\7z.exe"
  Delete "$INSTDIR\resources\7z\7z.dll"
  RMDir "$INSTDIR\resources\7z"
  RMDir "$INSTDIR\resources"

  ; 删除教程和模块
  RMDir /r "$INSTDIR\module_tutorial"
  RMDir /r "$INSTDIR\modules"

  ; 删除日志
  RMDir /r "$INSTDIR\logs"

  ; 删除快捷方式
  Delete "$DESKTOP\VortexDL.lnk"
  Delete "$SMPROGRAMS\VortexDL\VortexDL.lnk"
  Delete "$SMPROGRAMS\VortexDL\卸载 VortexDL.lnk"
  RMDir "$SMPROGRAMS\VortexDL"

  ; 删除注册表项
  DeleteRegKey HKLM "${APP_UNINSTALL_KEY}"

  ; 删除安装目录 (如果为空)
  RMDir "$INSTDIR"
SectionEnd

Function un.onInit
  MessageBox MB_YESNO|MB_ICONQUESTION "您确定要完全卸载 VortexDL 吗?$\n$\n所有安装的文件和快捷方式都将被删除。" IDYES +2
  Abort
FunctionEnd

Function un.onUnInstSuccess
  MessageBox MB_OK|MB_ICONINFORMATION "VortexDL 已成功卸载。"
FunctionEnd
