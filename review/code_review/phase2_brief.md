# Phase 2 fix brief (screen_fix code-quality remediation)

Repo: `S:\myProgr\proj\rust\screen_fix`. A code-quality review produced findings. They are stored in the files `findings_A.md`, `findings_B.md`, `findings_ECH.md`, `findings_DF.md` and `findings_G.md`, in the same scratchpad directory as this brief.

Phase 1 is already done. It moved the cross-cutting helpers into shared places:
- protocol predicates: `required_ready`, `valid`, `PhysicalRect` size/intersects, `ContextProducer::ALL`;
- `io_error` now lives in `platform/windows.rs`;
- the wide-string and `move_file` helpers;
- the `injected_hook` reuse;
- the HID usage constants;
- the VK hotkey helpers;
- `CaptureError` visibility, the native shared focus/process helpers, and the narrowed pub modules.

Build on that. Do not redo it.

## User decisions (2026-11-18)
- **Every `bugfix` finding:** apply it.
- **Every `none` finding:** apply it, including the large refactors:
  - split long functions into well-named steps;
  - replace stringly-typed contracts with types: the `ProbeError` kind (A7), the stills `SlotOutcome`/`DropReason` (B9), the grant and read state enums (G14), `SuppressionReason` (D9).
  
  Keep the serialized output byte-for-byte identical.
- **The `change` findings the user APPROVED:** E1, D1 (including revoke-instead-of-F for a loss after `after`), D13, G2 (unify at 750 ms CLEANUP, fix the comment), C3, H10, B5, C7, A4, G3, G18, B8, F16 (`div_euclid` in `scale_delta`), and the G7 witness `MsgWaitForMultipleObjectsEx`.

## Rules
- **Verify before you fix.** Re-read the code for each finding. If a finding is wrong or already fixed, skip it and say so. Never weaken a privacy, approval, security or trust check.
- **Behaviour.**
  - For `none` items, change no behaviour.
  - For `bugfix` and approved `change` items, change only what the finding describes.
  - Keep every serialized field name and value unchanged unless the finding explicitly requires a schema-conformant change (B3, G3).
  - Do not change any wire layout or `attachment::VERSION`.
- **Professional, idiomatic Rust.**
  - Minimal visibility, no dead code, and no new duplication.
  - Name constants, and keep doc comments accurate.
  - No `unwrap`/`expect` on fallible paths.
  - Give `unsafe` a correct safety argument.
- **Line endings and editing.** Keep LF line endings. Use Edit, or small exact-match replacements.
- **Ownership.** Edit ONLY the files your area owns (listed in your prompt). Other agents edit the other files in parallel. If a fix truly needs a change in a file you do not own, describe the exact change in your report instead of making it.
- **Commands.**
  - Do NOT run `cargo`: other agents are editing the same workspace, so a build would see half-finished states. One `cargo check` runs after all agents finish, so read signatures and imports carefully and keep every edit compile-safe.
  - Do not run git, except a read-only `git diff`.
- **Docs.** If a fix changes documented behaviour, list the doc update needed in `doc/*.md` in your report. Do not edit docs.

## Report (final message, English)
- Per finding id: done, skipped (with the reason), or needs another area (give the exact change).
- Any compile uncertainty.
- The doc updates needed.
