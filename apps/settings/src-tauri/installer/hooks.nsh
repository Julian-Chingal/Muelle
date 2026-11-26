; The dock (muelle.exe) is a bundled sidecar, not the Tauri main binary, so the
; installer stops and starts it itself.
!include "${__FILEDIR__}\taskbar.nsh"

; Quits the dock, gives the Windows taskbar back and waits for every
; muelle.exe (dock and watchdog) to exit, so its file can be replaced.
!macro MUELLE_STOP_DOCK
  ${If} ${FileExists} "$INSTDIR\muelle.exe"
    ExecWait '"$INSTDIR\muelle.exe" --quit'
  ${EndIf}
  StrCpy $MuelleDockStopped 0
  StrCpy $R8 0
  ${Do}
    nsis_tauri_utils::FindProcessCurrentUser "muelle.exe"
    Pop $R9
    ${If} $R9 <> 0
      StrCpy $MuelleDockStopped 1
      ${Break}
    ${EndIf}
    ${If} $R8 >= 50
      ; Still running after 5 s: force it, then verify it really stopped.
      nsis_tauri_utils::KillProcessCurrentUser "muelle.exe"
      Pop $R9
      Sleep 300
      nsis_tauri_utils::FindProcessCurrentUser "muelle.exe"
      Pop $R9
      ${If} $R9 <> 0
        StrCpy $MuelleDockStopped 1
      ${EndIf}
      ${Break}
    ${EndIf}
    Sleep 100
    IntOp $R8 $R8 + 1
  ${Loop}
  StrCpy $MuelleTaskbarRestored 0
  ${If} $MuelleDockStopped = 1
    !insertmacro MUELLE_RESTORE_TASKBAR
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro MUELLE_STOP_DOCK
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Updates keep whatever the user chose in Settings.
  ${If} $UpdateMode <> 1
    ExecWait '"$INSTDIR\muelle.exe" --autostart on'
  ${EndIf}
  ; Unelevated, even if the installer was run as administrator.
  nsis_tauri_utils::RunAsUser "$INSTDIR\muelle.exe" ""
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro MUELLE_STOP_DOCK
  ${If} $MuelleTaskbarRestored <> 1
    SetErrorLevel 2
    Abort "Windows taskbar restoration could not be verified. Uninstall stopped before removing Muelle or recovery data. Retry when Explorer is responding."
  ${EndIf}
  ; Removes the logon task (and an older Run entry), which would otherwise
  ; keep pointing at the deleted muelle.exe. Updates keep it.
  ${If} $UpdateMode <> 1
  ${AndIf} ${FileExists} "$INSTDIR\muelle.exe"
    ExecWait '"$INSTDIR\muelle.exe" --autostart off'
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    SetShellVarContext current
    ; Never erase a recovery record unless restoration has been confirmed.
    ${If} $MuelleTaskbarRestored = 1
      RmDir /r "$LOCALAPPDATA\Muelle"
    ${EndIf}
    ; Where Muelle remembers that it starts with Windows.
    DeleteRegKey HKCU "Software\Muelle"
    ${If} $DeleteAppDataCheckboxState = 1
      RmDir /r "$APPDATA\Muelle"
    ${EndIf}
  ${EndIf}
!macroend
