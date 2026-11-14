; SnapIt installer, all users (Program Files). Compiled by
; scripts/bundle-windows.sh:
;   makensis -DVERSION=.. -DVERSIONQUAD=.. -DBIN=.. -DICON=.. -DOUT=.. snapit.nsi

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef VERSIONQUAD
  !define VERSIONQUAD "0.0.0.0"
!endif
!ifndef BIN
  !error "BIN is required"
!endif
!ifndef ICON
  !define ICON "..\..\assets\snapit.ico"
!endif
!ifndef OUT
  !define OUT "SnapIt-setup.exe"
!endif

Name "SnapIt"
OutFile "${OUT}"
InstallDir "$PROGRAMFILES64\SnapIt"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
BrandingText "SnapIt"

VIProductVersion "${VERSIONQUAD}"
VIAddVersionKey "ProductName" "SnapIt"
VIAddVersionKey "FileDescription" "SnapIt setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Apache-2.0"

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "SimpChinese"

LangString DESC_Main ${LANG_ENGLISH} "SnapIt application (required)."
LangString DESC_Main ${LANG_SIMPCHINESE} "SnapIt 主程序（必需）。"
LangString DESC_Desktop ${LANG_ENGLISH} "Create a desktop shortcut."
LangString DESC_Desktop ${LANG_SIMPCHINESE} "创建桌面快捷方式。"
LangString MSG_RUNNING ${LANG_ENGLISH} "SnapIt is running. Setup must close it to continue. Close it and continue?"
LangString MSG_RUNNING ${LANG_SIMPCHINESE} "SnapIt 正在运行，需要先关闭才能继续。是否关闭并继续？"

; `tasklist /FO CSV` prints one quoted record per match. The "no tasks" notice is a
; localized INFO line that never starts with a quote, so check the first character.
!macro EnsureSnapItClosed
  nsExec::ExecToStack '"$SYSDIR\tasklist.exe" /FI "IMAGENAME eq SnapIt.exe" /NH /FO CSV'
  Pop $0
  Pop $1
  StrCpy $2 $1 1
  ${If} $2 == '"'
    MessageBox MB_OKCANCEL|MB_ICONEXCLAMATION "$(MSG_RUNNING)" /SD IDOK IDOK +2
    Abort
    nsExec::Exec '"$SYSDIR\taskkill.exe" /IM SnapIt.exe /F'
    Pop $0
    Sleep 800
  ${EndIf}
!macroend

Function .onInit
  SetRegView 64
  ; Remember the directory picked last time; an untouched default stays as-is.
  ReadRegStr $0 HKLM "Software\SnapIt" "InstallDir"
  ${If} $0 != ""
    StrCpy $INSTDIR $0
  ${EndIf}
  !insertmacro EnsureSnapItClosed
FunctionEnd

Function un.onInit
  SetRegView 64
  !insertmacro EnsureSnapItClosed
FunctionEnd

Section "SnapIt" SecMain
  SectionIn RO
  SetShellVarContext all
  SetRegView 64
  SetOutPath "$INSTDIR"
  File "/oname=SnapIt.exe" "${BIN}"
  File "/oname=SnapIt.ico" "${ICON}"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\SnapIt"
  CreateShortcut "$SMPROGRAMS\SnapIt\SnapIt.lnk" "$INSTDIR\SnapIt.exe" "" "$INSTDIR\SnapIt.exe" 0
  CreateShortcut "$SMPROGRAMS\SnapIt\Uninstall SnapIt.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKLM "Software\SnapIt" "InstallDir" "$INSTDIR"
  !define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\SnapIt"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "SnapIt"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "SnapIt"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\SnapIt.ico"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "EstimatedSize" $0
SectionEnd

Section "Desktop shortcut" SecDesktop
  ; Silent runs are in-app updates; do not recreate a shortcut the user deleted.
  IfSilent desktop_done
  SetShellVarContext all
  CreateShortcut "$DESKTOP\SnapIt.lnk" "$INSTDIR\SnapIt.exe" "" "$INSTDIR\SnapIt.exe" 0
  desktop_done:
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} $(DESC_Main)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} $(DESC_Desktop)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  SetShellVarContext all
  SetRegView 64
  Delete "$INSTDIR\SnapIt.exe"
  Delete "$INSTDIR\SnapIt.ico"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$DESKTOP\SnapIt.lnk"
  Delete "$SMPROGRAMS\SnapIt\SnapIt.lnk"
  Delete "$SMPROGRAMS\SnapIt\Uninstall SnapIt.lnk"
  RMDir "$SMPROGRAMS\SnapIt"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\SnapIt"
  DeleteRegKey HKLM "Software\SnapIt"
  ; Launch at login is per user; remove it when the uninstaller ran as that user.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "SnapIt"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "SnapIt"
SectionEnd
