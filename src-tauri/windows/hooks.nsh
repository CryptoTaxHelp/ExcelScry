; ---------------------------------------------------------------------------
;  ExcelScry - NSIS installer hooks
;
;  Adds the "Scry Excel File" context-menu verb, registers the ExcelScry
;  ProgIDs, and advertises the app to Windows' Default Apps page.
;
;  This file is included by Tauri's generated installer. The four hook macros
;  below are the supported insertion points.
;
;  IMPORTANT: the registry layout written here is mirrored by
;  src-tauri/src/associations.rs (REG_* / register_ops), which does the same
;  thing at runtime for the portable build. Change both together.
;
;  A note on "make it the default viewer": since Windows 8 the per-user key
;  Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\<ext>\UserChoice
;  is hash-protected, so no installer can silently take over an existing
;  association - Windows ignores a write it cannot verify. What this installer
;  does instead is everything that IS supported:
;    * registers a ProgID and advertises it via OpenWithProgids,
;    * adds the context-menu verb under SystemFileAssociations so it is always
;      available regardless of which app currently owns the extension,
;    * writes the legacy per-hive class default (honoured on profiles with no
;      UserChoice yet), backing up the previous value first,
;    * registers Capabilities so ExcelScry appears in Settings > Default apps,
;      where the user can make it the default in one click.
; ---------------------------------------------------------------------------

!include "LogicLib.nsh"

!define SCRY_APP_NAME "ExcelScry"
!define SCRY_EXE "ExcelScry.exe"
!define SCRY_VERB_KEY "ScryExcelFile"
!define SCRY_VERB_LABEL "Scry Excel File"
!define SCRY_CAPABILITIES "Software\ExcelScry\ExcelScry\Capabilities"
!define SCRY_BACKUP "Software\ExcelScry\Backup"
; NSIS's own Library.nsh defines SHCNE_ASSOCCHANGED (to this same value), and
; redefining it aborts the script. Keep our own name so this file does not
; depend on which includes the generated installer happens to pull in first.
!define SCRY_SHCNE_ASSOCCHANGED 0x08000000

; The classes Windows searches, in the order Explorer does.
!define SCRY_EXT1 ".xlsx"
!define SCRY_PROGID1 "ExcelScry.xlsx"
!define SCRY_DESC1 "ExcelScry Worksheet"

!define SCRY_EXT2 ".xlsm"
!define SCRY_PROGID2 "ExcelScry.xlsm"
!define SCRY_DESC2 "ExcelScry Macro-Enabled Workbook"

!define SCRY_EXT3 ".xls"
!define SCRY_PROGID3 "ExcelScry.xls"
!define SCRY_DESC3 "ExcelScry 97-2003 Worksheet"

; Tell Explorer that associations changed, so an already-running Explorer
; stops serving a stale context menu.
!macro ScryNotifyShell
  System::Call 'shell32::SHChangeNotify(i ${SCRY_SHCNE_ASSOCCHANGED}, i 0, i 0, i 0)'
!macroend

; Register one extension: ProgID, alternative-handler advert, context-menu
; verb, Default Apps capability, and the backed-up legacy default.
; ${ROOT} is a registry root (SHCTX at install, HKLM/HKCU at uninstall).
!macro ScryAssociate ROOT EXT PROGID DESC
  ; ---- ProgID ------------------------------------------------------------
  WriteRegStr ${ROOT} "Software\Classes\${PROGID}" "" "${DESC}"
  WriteRegStr ${ROOT} "Software\Classes\${PROGID}" "FriendlyTypeName" "${DESC}"
  WriteRegStr ${ROOT} "Software\Classes\${PROGID}\DefaultIcon" "" '"$INSTDIR\${SCRY_EXE}",0'
  WriteRegStr ${ROOT} "Software\Classes\${PROGID}\shell\open" "" "Open with ${SCRY_APP_NAME}"
  WriteRegStr ${ROOT} "Software\Classes\${PROGID}\shell\open\command" "" '"$INSTDIR\${SCRY_EXE}" "%1"'

  ; ---- Advertise as an alternative handler -------------------------------
  ; An empty REG_SZ here is read as "program is a candidate", which puts
  ; ExcelScry on the Open-with list without stealing the association.
  WriteRegStr ${ROOT} "Software\Classes\${EXT}\OpenWithProgids" "${PROGID}" ""

  ; ---- Context menu: "Scry Excel File" -----------------------------------
  ; Registered under SystemFileAssociations rather than under the ProgID, so
  ; the entry survives the user changing their default application.
  WriteRegStr ${ROOT} "Software\Classes\SystemFileAssociations\${EXT}\shell\${SCRY_VERB_KEY}" "" "${SCRY_VERB_LABEL}"
  WriteRegStr ${ROOT} "Software\Classes\SystemFileAssociations\${EXT}\shell\${SCRY_VERB_KEY}" "Icon" '"$INSTDIR\${SCRY_EXE}",0'
  WriteRegStr ${ROOT} "Software\Classes\SystemFileAssociations\${EXT}\shell\${SCRY_VERB_KEY}\command" "" '"$INSTDIR\${SCRY_EXE}" "%1"'

  ; ---- Default Apps registration ----------------------------------------
  WriteRegStr ${ROOT} "${SCRY_CAPABILITIES}\FileAssociations" "${EXT}" "${PROGID}"

  ; ---- Legacy default handler, backing up whatever was there -------------
  ; $0..$9 belong to the surrounding script, so preserve them across the read.
  Push $0
  Push $1
  ReadRegStr $1 ${ROOT} "${SCRY_BACKUP}\${EXT}" "HadValue"
  ; Only capture the original the first time; a re-install or upgrade must not
  ; record our own ProgID as the value to "restore".
  ${If} $1 == ""
    ReadRegStr $0 ${ROOT} "Software\Classes\${EXT}" ""
    ${If} $0 == ""
      WriteRegDWORD ${ROOT} "${SCRY_BACKUP}\${EXT}" "HadValue" 0
    ${Else}
      WriteRegDWORD ${ROOT} "${SCRY_BACKUP}\${EXT}" "HadValue" 1
    ${EndIf}
    WriteRegStr ${ROOT} "${SCRY_BACKUP}\${EXT}" "Value" "$0"
  ${EndIf}
  Pop $1
  Pop $0

  WriteRegStr ${ROOT} "Software\Classes\${EXT}" "" "${PROGID}"
!macroend

; Undo ScryAssociate for one extension, restoring the backed-up default.
!macro ScryDisassociate ROOT EXT PROGID
  DeleteRegKey ${ROOT} "Software\Classes\${PROGID}"
  DeleteRegValue ${ROOT} "Software\Classes\${EXT}\OpenWithProgids" "${PROGID}"
  ; Only our own verb is removed; other viewers' verbs are left alone.
  DeleteRegKey ${ROOT} "Software\Classes\SystemFileAssociations\${EXT}\shell\${SCRY_VERB_KEY}"
  DeleteRegValue ${ROOT} "${SCRY_CAPABILITIES}\FileAssociations" "${EXT}"

  Push $0
  Push $1
  ReadRegStr $0 ${ROOT} "Software\Classes\${EXT}" ""
  ; Undo the default only if it is still us - the user may have chosen
  ; something else in the meantime, and that choice must be preserved.
  ${If} $0 == "${PROGID}"
    ReadRegStr $1 ${ROOT} "${SCRY_BACKUP}\${EXT}" "HadValue"
    ${If} $1 == "1"
      ReadRegStr $1 ${ROOT} "${SCRY_BACKUP}\${EXT}" "Value"
      WriteRegStr ${ROOT} "Software\Classes\${EXT}" "" "$1"
    ${ElseIf} $1 == "0"
      DeleteRegValue ${ROOT} "Software\Classes\${EXT}" ""
    ${EndIf}
  ${EndIf}
  Pop $1
  Pop $0

  DeleteRegKey ${ROOT} "${SCRY_BACKUP}\${EXT}"
!macroend

!macro ScryRegisterAll ROOT
  !insertmacro ScryAssociate ${ROOT} "${SCRY_EXT1}" "${SCRY_PROGID1}" "${SCRY_DESC1}"
  !insertmacro ScryAssociate ${ROOT} "${SCRY_EXT2}" "${SCRY_PROGID2}" "${SCRY_DESC2}"
  !insertmacro ScryAssociate ${ROOT} "${SCRY_EXT3}" "${SCRY_PROGID3}" "${SCRY_DESC3}"

  WriteRegStr ${ROOT} "${SCRY_CAPABILITIES}" "ApplicationName" "${SCRY_APP_NAME}"
  WriteRegStr ${ROOT} "${SCRY_CAPABILITIES}" "ApplicationDescription" "ExcelScry - view XLS and XLSX spreadsheets, export to ODS"
  WriteRegStr ${ROOT} "Software\RegisteredApplications" "${SCRY_APP_NAME}" "${SCRY_CAPABILITIES}"
!macroend

!macro ScryUnregisterAll ROOT
  !insertmacro ScryDisassociate ${ROOT} "${SCRY_EXT1}" "${SCRY_PROGID1}"
  !insertmacro ScryDisassociate ${ROOT} "${SCRY_EXT2}" "${SCRY_PROGID2}"
  !insertmacro ScryDisassociate ${ROOT} "${SCRY_EXT3}" "${SCRY_PROGID3}"
  DeleteRegValue ${ROOT} "Software\RegisteredApplications" "${SCRY_APP_NAME}"
  ; Removes Capabilities together with any leftover backup state.
  DeleteRegKey ${ROOT} "Software\ExcelScry"
!macroend

; ---------------------------------------------------------------------------
;  Tauri hook entry points
; ---------------------------------------------------------------------------

!macro NSIS_HOOK_POSTINSTALL
  ; SHCTX follows the install mode (HKLM for perMachine, HKCU for currentUser),
  ; so the registration lands in the hive the installer actually owns.
  !insertmacro ScryRegisterAll SHCTX
  !insertmacro ScryNotifyShell
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Clean both hives explicitly rather than relying on SHCTX: the uninstaller
  ; changes the shell var context in places, and the portable build may have
  ; self-registered under HKCU even for a per-machine install.
  !insertmacro ScryUnregisterAll HKLM
  !insertmacro ScryUnregisterAll HKCU
  !insertmacro ScryNotifyShell
!macroend
