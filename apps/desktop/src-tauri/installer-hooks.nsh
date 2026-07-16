; ANVIL NSIS installer hooks (Tauri v2 `bundle.windows.nsis.installerHooks`,
; see tauri.conf.json). Macro names/timing are fixed by tauri-bundler's installer.nsi
; template — see crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi upstream.
;
; PREUNINSTALL runs inside `Section Uninstall`, before any files/registry keys/shortcuts
; are removed — $INSTDIR and ${MAINBINARYNAME}.exe still exist on disk at this point.
;
; Why this hook exists (05 §M5 "uninstall hygiene"): `anvil-core::platform`
; (src/platform/windows.rs, workspace crate `anvil-core`) writes its own per-user HKCU
; keys — an Explorer "Master with ANVIL" context-menu verb, an "Open with" list entry per
; supported extension, and a login autostart Run-key value — but ONLY when the user opts
; in from Settings (04 §S8 Integration toggles). The installer itself never wrote those
; keys (Tauri's own file-association registration is separate — see `fileAssociations` in
; tauri.conf.json — and is cleaned up automatically by the generated uninstaller), so the
; generated uninstall logic has no way to know about them. Running the app's own
; `--uninstall-cleanup` flag here removes them for real: each unregister call in
; `platform/windows.rs` is idempotent, so this is safe to run unconditionally on every
; uninstall even when the user never turned any of it on.
!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Removing ANVIL's Explorer integration and autostart entry..."
  ExecWait '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-cleanup'
!macroend
