# Phase 1: cross-cutting shared helpers (resume brief)

Phase 1 was stopped part-way on 2026-11-18. Items 1–6 appear done; items 7–13 have not been checked. The tree compiles cleanly. Check the state of each item before changing anything. Findings detail is in `review/code_review/findings_*.md`.

## Items
1. **F5.** `InputRegistration::required_ready()` and `EnvironmentState::valid()` in protocol; replace every open-coded copy.
2. **F7.** `config.rs` uses the protocol `MAX_APPROVED_EXECUTABLES`, plus a new protocol `MAX_APPROVAL_KEY_BYTES` (4096).
3. **F15.** Replace `UiaStatus` with `Result`/`Option<ReadUnavailable>`, only if it is not in any wire struct.
4. **D12.** `ContextProducer::ALL`/`index()`, used by core `elevated/ingress.rs` and elevated `runtime/outbox.rs`; comment the pre-consumed held position.
5. **A11 / B10.** `PhysicalRect` size helpers and `intersects()`. Use them in `video.rs`, `monitor.rs`, `preflight.rs` and `context.rs`.
6. **A14.** Move `io_error` into `platform/windows.rs`; set no native code when there is no OS error.
7. **B10.**
   - One wide-string helper and one `move_file(src, dst, replace)` in `platform/windows.rs`, with a single `last_os_error` mapping.
   - Replace the copies in `os.rs`, `still_png.rs`, `windows.rs`, `input_helper.rs`, `mp4.rs` and `elevated/launcher.rs`.
   - The elevated crate keeps its own copy.
8. **E5 / C8.**
   - Make `matching::injected_hook` `pub(crate)` and reuse it in `evidence.rs`, `held.rs` and `gate.rs`.
   - Name the constants LLKHF_UP, VK_PACKET and VK_CAPITAL.
   - Replace the AltGr 20_000 in `held.rs` with `MATCH_WINDOW_US`, after confirming it is the same quantity.
9. **C8 / H11.**
   - One source for the HID usage pairs: `OptionalCollection::usage()`, `HidReport::is_pen()`, and `is_touchpad()`.
   - Replace the hand-coded pairs in native-observation `raw.rs`, `device.rs`, `hid.rs`, and in core `gate.rs`, `text.rs`, `input.rs` and `input/hid.rs`.
10. **F8 / B16.**
    - Add `VK_F1`, `HotkeyBindings::virtual_key(action)` and `function_of(vk)`, and use them in `session/hotkey.rs`, `platform/windows/hotkeys.rs` and `ui/src/app.rs`.
    - Use the windows-sys VK modifier constants and a named `ToUnicodeEx` flag.
11. **F14.** `CaptureError` fields become `pub(crate)`/private, derived from `failure`. Check the `ui` crate first.
12. **H9.** Shared native helpers in native-observation:
    - `focus_at(thread, pid)`;
    - `process_instance(handle)`, the creation FILETIME;
    - the integrity RID.

    Use them in native-observation and in the elevated crate where the dependencies and features allow.
13. **E8.** Narrow to `pub(crate)`: `package::outcome`, `capture::{input, frame, context, observation}`, `InputOrigin` and `PointerEvidence`. Check `ui` first.

## Rules
- Refactor only: no behaviour change, no serialized name or value change, no change to the wire format or `attachment::VERSION`.
- Keep LF line endings.
- `cargo check --workspace --all-targets` is allowed. Run it at the end and fix until there are 0 errors and 0 warnings. No tests, clippy or fmt.
- Git: read-only `git diff` only.
