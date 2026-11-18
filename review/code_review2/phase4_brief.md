# Phase 4 brief: splitting god objects and functions (round 2)

**Repo:** `S:\myProgr\proj\rust\screen_fix`. Windows-only.

**Already committed:** phases 1–3 of round 2. That work added:
- shared primitives:
  - QPC/time methods in protocol;
  - `LossSpan`;
  - `core/src/util.rs`;
  - `native_observation::{owner, wake, com}`;
  - `StaticLease`;
  - error `From` conversions;
- typed journal rows: `package/row.rs`, `StillRow`, `DropReason`, `WindowRowKind`, `NativeStatus`, and fixed input rows;
- layer moves:
  - the controller moved to `platform/windows/controller*`;
  - pure input policy moved to `capture/input/*`;
  - `package/{source,finalize}.rs` were added.

**Tests.** Golden byte tests and package tests exist in `screen-fix-core`. They are all green; the user runs them.

**Read first:**
- `review/code_review2/AUDIT_SUMMARY.md` (theme T1);
- your area's findings in `review/code_review2/findings_<area>.md`;
- the X findings that touch your area.

## Goal
Make the big units easy to read and change. The means:
- split god structs into cohesive sub-structs, each with its own reset;
- split god functions into named steps;
- replace tuple, bool and `Option` state with small types and state enums;
- add the missing carrier types that remove long parameter lists.

## Rules
- **Behaviour.**
  - Refactor only (`none`). Keep the order of side effects, journal rows, locks and thread interactions exactly the same.
  - Serialized bytes must not change.
  - Do NOT change any expected value in an existing test. If a test no longer compiles because a signature changed, adapt the call and keep the expected value.
- **Safety.** Never weaken a privacy, approval, security or trust check. Keep every checkpoint in the same place.
- **Drop order.** Where the drop order of fields matters (it is documented in some structs, for example `Pump`), preserve it when you group fields, and comment where needed.
- **Code style.** Write idiomatic, minimal code:
  - no dead code;
  - minimal visibility;
  - accurate doc comments and SAFETY comments;
  - LF line endings and valid UTF-8.
- **Ownership.** Edit ONLY the files your area owns (see the table). Other agents work in parallel. Keep the public and crate API that other areas call stable. If an improvement needs a change in a file you do not own, describe the exact change in your report.
- **Commands.**
  - Do NOT run `cargo`. Other agents are editing the workspace at the same time. The coordinator runs one `cargo check` afterwards, so read signatures carefully and keep every edit compile-safe.
  - No tests. Git only for read-only commands.
  - The harness forbids subagents from writing report files, so put the report in your final message.
- **Size.** Prefer a few well-done structural splits over many cosmetic ones. If a split is too risky to do without compiling, do part of it, or skip it, and say why.

## File ownership
Paths are under `core/src/` unless stated otherwise.

| Area | Files |
|---|---|
| A | `platform/windows/{video, monitor, encode, encoder, h264, mp4, slots, readback, gpu, pixels, native, wgc, mf, hdr, clock, display*}` (with submodules) |
| B | `platform/windows/{stills, still_rows, still_png, still_time, clipboard, hotkeys, ui_window*, preflight, os, input_helper}.rs` and `platform/windows.rs`, `platform/mod.rs` |
| C | `platform/windows/input.rs`, `platform/windows/input/**`, and `capture/input/{hid, hid/touchpad, hook_health, blind, devices, publication, loss}.rs` |
| D | `platform/windows/context*` (with `context/**`), `platform/windows/elevated/**` |
| E | `capture/**` (except area C's files), `package/**` |
| F | `platform/windows/controller*` (with `controller/**`), `session*`, `session/**`, the top-level `*.rs` files (`lib`, `error`, `config`, `util`, `devlog`), `protocol/**`, `ui/**` |
| G | `core/capture/elevated/**` (outside `core/src`) |
| H | `core/capture/native-observation/**`, `core/capture/input/**` (outside `core/src`) |

## Report (final message, English, at most ~500 words)
- **Per finding id:** done, partly or skipped, with the reason.
- **Structural result:** the new types and modules, and the before/after size of the largest units.
- **Compile risks:** any spots you are unsure about.
- **Cross-area changes:** any you need, with the exact change.
- **Docs:** any docs that need an update.
