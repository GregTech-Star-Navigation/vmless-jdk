; vmless 安装包（NSIS 3.x）
; 由 packaging/build.ps1 调用，三个必需的定义：
;   /DSTAGE=<暂存目录>   含 bin\{rustjava,compile_src,run_jit}.exe + README.txt + LICENSE
;   /DVERSION=<版本号>
;   /DOUTFILE=<输出 exe 路径>
; 手动构建：makensis /INPUTCHARSET UTF8 /DSTAGE=..\dist\stage /DVERSION=0.1.0 /DOUTFILE=..\dist\vmless-0.1.0-setup.exe vmless.nsi

Unicode true
SetCompressor /SOLID lzma
SetCompressorDictSize 32

!include "MUI2.nsh"
!include "WinMessages.nsh"
!include "StrFunc.nsh"

; StrFunc 需要先声明再使用（生成函数体）；只声明用到的两个，否则 makensis 会报"未引用、清零"
${StrStr}
${UnStrRep}

!ifndef VERSION
    !define VERSION "0.0.0"
!endif
!ifndef STAGE
    !error "缺 -DSTAGE=<暂存目录>"
!endif
!ifndef OUTFILE
    !define OUTFILE "vmless-${VERSION}-setup.exe"
!endif

!define APP_NAME "vmless"
!define APP_KEY "Software\vmless"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\vmless"

Name "${APP_NAME} ${VERSION}"
BrandingText "${APP_NAME} ${VERSION}"
OutFile "${OUTFILE}"
; 装到用户目录，不需要管理员权限
InstallDir "$LOCALAPPDATA\vmless"
InstallDirRegKey HKCU "${APP_KEY}" "InstallDir"
RequestExecutionLevel user

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=2052 "ProductName" "vmless"
VIAddVersionKey /LANG=2052 "FileDescription" "vmless Java 工具链安装程序"
VIAddVersionKey /LANG=2052 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=2052 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=2052 "LegalCopyright" "vmless contributors"

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_NOAUTOCLOSE
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

; ------------------------------------------------------------------ 组件
Section "运行时（必需）" SEC_CORE
    SectionIn RO
    SetOutPath "$INSTDIR"
    SetOutPath "$INSTDIR\bin"
    File "${STAGE}\bin\rustjava.exe"
    File "${STAGE}\bin\compile_src.exe"
    File "${STAGE}\bin\run_jit.exe"
    SetOutPath "$INSTDIR"
    File "${STAGE}\README.txt"
    File "${STAGE}\LICENSE"

    WriteUninstaller "$INSTDIR\uninstall.exe"

    WriteRegStr HKCU "${APP_KEY}" "InstallDir" "$INSTDIR"
    WriteRegStr HKCU "${APP_KEY}" "Version" "${VERSION}"
    WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${APP_NAME} ${VERSION}"
    WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
    WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "vmless"
    WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\bin\rustjava.exe"
    WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
    WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
    WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
    WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "把 bin 加入当前用户 PATH" SEC_PATH
    Call AddToPath
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
!insertmacro MUI_DESCRIPTION_TEXT ${SEC_CORE} "rustjava / compile_src / run_jit 三个可执行文件，不能取消。"
!insertmacro MUI_DESCRIPTION_TEXT ${SEC_PATH} "勾选后在任意目录可直接敲 rustjava；卸载时会一并移除。"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ------------------------------------------------------------------ PATH
Function AddToPath
    StrCpy $1 "$INSTDIR\bin"
    ReadRegStr $0 HKCU "Environment" "Path"
    ${StrStr} $2 "$0" "$1"
    StrCmp $2 "" 0 done                 ; 已经有这条，别再重复
    StrCmp $0 "" 0 append
        WriteRegExpandStr HKCU "Environment" "Path" "$1"
        Goto done
    append:
        StrCpy $3 "$0" 1 -1             ; 末字符
        StrCmp $3 ";" 0 +2
            StrCpy $0 "$0" -1           ; 去掉末尾多余的分号
        StrCpy $0 "$0;$1"
        WriteRegExpandStr HKCU "Environment" "Path" "$0"
    done:
    SendMessage ${HWND_BROADCAST} ${WM_WININICHANGE} 0 "STR:Environment" /TIMEOUT=3000
FunctionEnd

Section "Uninstall"
    SetShellVarContext current

    Delete "$INSTDIR\bin\rustjava.exe"
    Delete "$INSTDIR\bin\compile_src.exe"
    Delete "$INSTDIR\bin\run_jit.exe"
    RMDir "$INSTDIR\bin"
    Delete "$INSTDIR\README.txt"
    Delete "$INSTDIR\LICENSE"
    Call un.RemoveFromPath
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"

    DeleteRegKey HKCU "${UNINST_KEY}"
    DeleteRegKey HKCU "${APP_KEY}"
SectionEnd

Function un.RemoveFromPath
    StrCpy $1 "$INSTDIR\bin"
    ReadRegStr $0 HKCU "Environment" "Path"
    StrCmp $0 "" done
    ${UnStrRep} $0 "$0" ";$1" ""
    ${UnStrRep} $0 "$0" "$1;" ""
    ${UnStrRep} $0 "$0" "$1" ""
    WriteRegExpandStr HKCU "Environment" "Path" "$0"
    SendMessage ${HWND_BROADCAST} ${WM_WININICHANGE} 0 "STR:Environment" /TIMEOUT=3000
    done:
FunctionEnd
