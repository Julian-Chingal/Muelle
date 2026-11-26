; Restoration belongs to the uninstaller too: muelle.exe may have been removed.
; All calls use the installer's pointer width, not the OS's architecture.
Var MuelleTaskbarRestored
Var MuelleDockStopped

!if ${NSIS_PTR_SIZE} > 4
  !define MUELLE_GETSTYLE GetWindowLongPtrW
  !define MUELLE_SETSTYLE SetWindowLongPtrW
!else
  !define MUELLE_GETSTYLE GetWindowLongW
  !define MUELLE_SETSTYLE SetWindowLongW
!endif

!macro MUELLE_REVEAL_TASKBAR
  System::Call 'user32::${MUELLE_GETSTYLE}(p r1, i -20)p.r3'
  IntOp $3 $3 & 0x00080020
  ${If} $3 <> 0
    System::Call 'user32::SetLayeredWindowAttributes(p r1, i 0, i 255, i 2)i'
    System::Call 'user32::${MUELLE_GETSTYLE}(p r1, i -20)p.r3'
    IntOp $3 $3 & 0xFFF7FFDF
    System::Call 'user32::${MUELLE_SETSTYLE}(p r1, i -20, p r3)p'
  ${EndIf}
  System::Call 'user32::${MUELLE_GETSTYLE}(p r1, i -20)p.r3'
  IntOp $3 $3 & 0x00080020
  System::Call 'user32::IsWindow(p r1)i.r4'
  ${If} $3 <> 0
  ${OrIf} $4 = 0
    StrCpy $MuelleTaskbarRestored 0
  ${EndIf}
!macroend

!macro MUELLE_RESTORE_TASKBAR
  ; Preserve the calling installer's registers; the result has its own variable.
  System::Store "S"
  StrCpy $MuelleTaskbarRestored 1
  ; No record means Muelle never took over, or its verified restorer already
  ; completed. Do not alter another desktop tool's styles or guess a preference.
  ${If} ${FileExists} "$LOCALAPPDATA\Muelle\taskbar-original"
    System::Call 'user32::FindWindowW(w "Shell_TrayWnd", p 0)p.r1'
    StrCpy $5 $1
    ${If} $1 <> 0
      !insertmacro MUELLE_REVEAL_TASKBAR
    ${EndIf}
    StrCpy $1 0
    ${Do}
      System::Call 'user32::FindWindowExW(p 0, p r1, w "Shell_SecondaryTrayWnd", p 0)p.r1'
      ${If} $1 = 0
        ${Break}
      ${EndIf}
      !insertmacro MUELLE_REVEAL_TASKBAR
    ${Loop}
    ClearErrors
    FileOpen $0 "$LOCALAPPDATA\Muelle\taskbar-original" r
    ${If} ${Errors}
      StrCpy $MuelleTaskbarRestored 0
    ${Else}
      StrCpy $6 ""
      ClearErrors
      FileRead $0 $6
      ${If} ${Errors}
        StrCpy $6 "invalid"
      ${EndIf}
      FileClose $0
      ${If} $6 != "0"
      ${AndIf} $6 != "1"
        StrCpy $MuelleTaskbarRestored 0
      ${ElseIf} $5 = 0
        ; Explorer is not ready: keep the backup for a retry.
        StrCpy $MuelleTaskbarRestored 0
      ${Else}
        StrCpy $1 $5
        !if ${NSIS_PTR_SIZE} > 4
          System::Call '*(i 48, i 0, p r1, i 0, i 0, i 0, i 0, i 0, i 0, p 0)p.r2'
        !else
          System::Call '*(i 36, p r1, i 0, i 0, i 0, i 0, i 0, i 0, p 0)p.r2'
        !endif
        ${If} $2 = 0
          StrCpy $MuelleTaskbarRestored 0
        ${Else}
          System::Call 'shell32::SHAppBarMessage(i 4, p r2)p.r3'
          IntOp $3 $3 & 0xFFFFFFFE
          IntOp $3 $3 | $6
          !if ${NSIS_PTR_SIZE} > 4
            System::Call '*$2(i 48, i 0, p r1, i 0, i 0, i 0, i 0, i 0, i 0, p r3)'
          !else
            System::Call '*$2(i 36, p r1, i 0, i 0, i 0, i 0, i 0, i 0, p r3)'
          !endif
          System::Call 'shell32::SHAppBarMessage(i 10, p r2)p'
          ; SETSTATE always returns true; GETSTATE is the confirmation.
          System::Call 'shell32::SHAppBarMessage(i 4, p r2)p.r3'
          System::Free $2
          IntOp $3 $3 & 1
          System::Call 'user32::IsWindow(p r1)i.r4'
          ${If} $3 != $6
          ${OrIf} $4 = 0
            StrCpy $MuelleTaskbarRestored 0
          ${EndIf}
        ${EndIf}
      ${EndIf}
    ${EndIf}
    ${If} $MuelleTaskbarRestored = 1
      ClearErrors
      Delete "$LOCALAPPDATA\Muelle\taskbar-original"
      ${If} ${Errors}
        StrCpy $MuelleTaskbarRestored 0
      ${EndIf}
    ${EndIf}
  ${EndIf}
  System::Store "L"
!macroend
