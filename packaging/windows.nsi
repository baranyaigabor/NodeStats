Unicode True
Name "NodeStats"
OutFile "${OUTPUT}"
InstallDir "$LOCALAPPDATA\Programs\NodeStats"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!define MUI_FINISHPAGE_RUN_FUNCTION StartSetup
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Pair NodeStats and enable startup"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function StartSetup
    ExecShell "open" "$INSTDIR\nodestats.exe" "setup"
FunctionEnd

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\NodeStats"

Section "NodeStats"
    SetOutPath "$INSTDIR"
    File /oname=nodestats.exe "${AGENT}"
    WriteUninstaller "$INSTDIR\uninstall.exe"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "NodeStats"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\nodestats.exe"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
SectionEnd

Section "Uninstall"
    ExecWait '"$INSTDIR\nodestats.exe" unregister-startup' $0
    ${If} $0 != 0
        MessageBox MB_ICONSTOP "NodeStats could not remove its startup task."
        Abort
    ${EndIf}
    Delete "$INSTDIR\nodestats.exe"
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"
    DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
