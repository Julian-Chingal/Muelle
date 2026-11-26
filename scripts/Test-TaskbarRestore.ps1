# Verify uninstall ordering and compile the real hooks. Never run an uninstaller
# or change the current user's Explorer/taskbar preferences.
[CmdletBinding()]
param([string]$MakeNsis)
$ErrorActionPreference = 'Stop'
$hooksPath = (Resolve-Path (Join-Path $PSScriptRoot '../apps/settings/src-tauri/installer/hooks.nsh')).Path
$hooks = Get-Content -LiteralPath $hooksPath -Raw
$restorePath = Join-Path (Split-Path $hooksPath) 'taskbar.nsh'
$restore = Get-Content -LiteralPath $restorePath -Raw
if ($hooks.Contains('--restore') -or -not $hooks.Contains('--quit') -or
    $hooks.IndexOf('FindProcessCurrentUser') -gt $hooks.IndexOf('!insertmacro MUELLE_RESTORE_TASKBAR')) {
    throw 'Stop and wait for the dock/guard before restoring; never restore alongside a running dock.'
}
$pre = $hooks.Substring($hooks.IndexOf('!macro NSIS_HOOK_PREUNINSTALL'))
if ($pre.IndexOf('Abort ') -lt 0 -or $pre.IndexOf('Abort ') -gt $pre.IndexOf('--autostart off')) {
    throw 'Uninstall must stop before deleting app files when restoration cannot be confirmed.'
}
$set = $restore.IndexOf('SHAppBarMessage(i 10')
$verify = $restore.LastIndexOf('SHAppBarMessage(i 4')
$delete = $restore.IndexOf('Delete "')
if ($set -lt 0 -or $verify -le $set -or $delete -le $verify) {
    throw 'Verify GETSTATE after SETSTATE, before deleting the saved preference.'
}
$cleanup = $hooks.Substring($hooks.IndexOf('!macro NSIS_HOOK_POSTUNINSTALL'))
if ($cleanup -notmatch '(?s)\$MuelleTaskbarRestored = 1\s+RmDir /r "\$LOCALAPPDATA\\Muelle"\s+\$\{EndIf\}') {
    throw 'Recovery directory cleanup must require confirmed restoration.'
}
if ($restore -notmatch '\$3 != \$6' -or $restore -notmatch '\$6 != "0"' -or
    $restore -notmatch '\$6 != "1"' -or $restore -notmatch 'i 48' -or $restore -notmatch 'i 36') {
    throw 'Restore must preserve both original modes, reject corrupt records and use the native structure size.'
}
Write-Host 'PASS: uninstall waits, verifies both preferences and preserves the backup on failure'
# APPBARDATA includes both uCallbackMessage AND uEdge before RECT. NSIS/System
# does not check C layout: validate every allocation/write against the SDK ABI.
foreach ($layout in [regex]::Matches($restore, "\*[^']*?\((i (?:36|48),[^)]+)\)")) {
    $fields = $layout.Groups[1].Value.Split(',').Trim()
    $size = [int]($fields[0] -replace '^i ', '')
    $pointer = if ($size -eq 48) { 8 } else { 4 }
    $bytes = @($fields | ForEach-Object { if ($_ -match '^p ') { $pointer } else { 4 } })
    $actual = ($bytes | Measure-Object -Sum).Sum
    $offset = $actual - $bytes[-1]
    $expectedOffset = if ($pointer -eq 8) { 40 } else { 32 }
    if ($actual -ne $size -or $offset -ne $expectedOffset) { throw 'APPBARDATA allocation or lParam offset differs from the Windows SDK.' }
}
if ([regex]::Matches($restore, "\*[^']*?\((i (?:36|48),[^)]+)\)").Count -ne 4) { throw 'Expected allocation and update layouts for both pointer widths.' }
Write-Host 'PASS: 32/64-bit APPBARDATA sizes and lParam offsets match the Windows SDK'
if (-not $MakeNsis) {
    $command = Get-Command makensis -ErrorAction SilentlyContinue
    if ($command) { $MakeNsis = $command.Source }
    else { $MakeNsis = Join-Path $env:LOCALAPPDATA 'tauri/NSIS/makensis.exe' }
}
if (-not (Test-Path -LiteralPath $MakeNsis -PathType Leaf)) {
    Write-Host 'SKIP: NSIS compiler unavailable; source contract checked, native compilation not run'
    exit 0
}
$pluginDir = Join-Path (Split-Path $MakeNsis) 'Plugins/x86-unicode/additional'
$root = Join-Path ([IO.Path]::GetTempPath()) ('muelle-taskbar-compile-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $root | Out-Null
try {
    $fixture = @'
Unicode true
RequestExecutionLevel user
!include LogicLib.nsh
!include FileFunc.nsh
!addplugindir "__PLUGINS__"
!include "__HOOKS__"
Name "Muelle taskbar hook compile check"
OutFile "__OUTPUT__"
Var UpdateMode
Var DeleteAppDataCheckboxState
; Syntax-check every real hook, but never call this function or execute the file.
Function CheckHooks
  !insertmacro NSIS_HOOK_PREINSTALL
  !insertmacro NSIS_HOOK_POSTINSTALL
  !insertmacro NSIS_HOOK_PREUNINSTALL
  !insertmacro NSIS_HOOK_POSTUNINSTALL
FunctionEnd
Section
SectionEnd
'@
    $fixture = $fixture.Replace('__PLUGINS__', $pluginDir).Replace('__HOOKS__', $hooksPath).
        Replace('__OUTPUT__', (Join-Path $root 'compile-only.exe'))
    $inputFile = Join-Path $root 'fixture.nsi'
    Set-Content -LiteralPath $inputFile -Value $fixture -Encoding utf8NoBOM
    & $MakeNsis /V2 $inputFile
    if ($LASTEXITCODE -ne 0) { throw 'NSIS hook compilation failed.' }
    Write-Host 'PASS: all install/uninstall hooks compile with the real NSIS/System/Tauri plugins; no executable run'
} finally {
    $resolved = [IO.Path]::GetFullPath($root)
    $temp = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if (-not $resolved.StartsWith($temp, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($resolved) -notlike 'muelle-taskbar-compile-*') { throw 'Unsafe fixture cleanup path.' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
