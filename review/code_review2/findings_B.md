# Findings, area B: stills and the platform shell (round 2)

Paths are relative to `core/src/platform/`. Items done in round 1 are not repeated, and neither are the ones it skipped with a reason (B10 counters, B11 polling).

Counts: high 0, medium 4, low 13. Every finding has behaviour `none`.

1. **B1** · medium · Architecture (god module / god struct)
   - **Where:** windows/stills.rs (1651 lines); `Stills` fields at 616-638 (21 fields); `Requests` at 169-177.
   - **Problem:** six concepts live in one file:
     - the slot and row model;
     - private openings (`EarlyRows`, `Private`);
     - scroll bursts;
     - quiet/settled tracking;
     - shared readback and PNG work (`FrameWork`);
     - hold reclaim.

     `Stills` itself mixes command intake (two queues, two deferred slots, three `Arc<AtomicI64>`), the observe timer, quiet state, scrolls and queues.
   - **Fix:** turn the file into a `stills/` directory:
     - `slot.rs`: `Public`, `State`, `SlotOutcome`, `DropReason`, rows;
     - `private.rs`;
     - `scroll.rs`;
     - `settle.rs`: a `Quiet` tracker;
     - `work.rs`: `FrameWork`, `attach`, `start_readback`, `advance_slots`;
     - `mod.rs`.

     Fold the three atomics into one `Arc<Signals>`.
   - **Size:** L (mechanical) · **Behaviour:** none

2. **B2** · medium · Hidden coupling / DRY
   - **Where:** stills.rs:457-489, 1638-1651; still_rows.rs:99-110, 114-122, 380-386; stills.rs:478-480 and windows/input.rs:1621-1622.
   - **Problem:** the still row (schema §6) is a `serde_json::Value` built twice, in `row_value` and `fallback_row`. `still_rows` then edits it by string key:
     - `publishable` sets 7 fields to null and rewrites `kind`, `reason` and `outcome`;
     - `with_seen` copies keys;
     - the deadline path moves `reason` into `fallback_reason`.

     `input.rs` patches `fallback["save"]` with its own second builder. One field change touches 4 places in 3 files, and a typo still compiles.
   - **Fix:**
     - One `#[derive(Serialize)] struct StillRow`, with `skip_serializing_if` exactly where keys are absent today.
     - Methods on it: `fallback()`, `withdraw_uncertified()`, `join_seen()`, `deadline()`.
     - A `SaveRef` newtype for the save reference.
     - Serialize once, at submit.
     - A test that the JSON is unchanged.
   - **Size:** M · **Behaviour:** none (identical JSON)

3. **B3** · medium · Stringly typed (round-1 B9 is unfinished)
   - **Where:** stills.rs:359-360 (`DropReason::Drain(&str)`), 1483, 1640; monitor.rs:708; input.rs:1620; receiver.rs:1302, 1347; still_rows.rs:105-106, 384.
   - **Problem:** 9 values of the same `reason` field are still string literals, spread over 4 files outside `DropReason`. Two of them duplicate existing variants (`elevated_frame_uncertified`, `still_owner_closed`). `Drain(&str)` keeps the enum open.
   - **Fix:**
     - Add the variants `StopDrainDeadline`, `PauseDrainDeadline`, `SaveHandoffUnavailable`, `PointerRestHandoffUnavailable`, `ProvisionalRetentionUnavailable`, `MonitorRoutingUnknown` and `StillOrderedDeadline`.
     - Delete `Drain`.
     - Make `DropReason` `pub(super)`.
     - `fallback_row` and `approve` take a `DropReason`; `expire_drain` takes `stopping: bool`.
   - **Size:** S · **Behaviour:** none

4. **B4** · medium · God functions / layering
   - **Where:** windows.rs:221-440 (`record_with_disk`), 505-710 (`record`).
   - **Problem:**
     - `record_with_disk` mixes journal spawning, INP-13 blind and health rows, journal closing, the producer prefix check, diagnostics and metadata, publishing, and the completion text. Most of this is controller or package policy, yet it lives in the platform shell.
     - `record` mixes PNG and reporter setup, the monitor spawn and readiness wait, the input run, and the joins.
   - **Fix:**
     - Extract `input_final_rows`, `close_journals`, `verify_prefixes` and `completion_message`, and move the last two into `session`.
     - In `record`, extract `spawn_reporter`, `spawn_monitors` and `join_monitors`.
   - **Size:** M · **Behaviour:** none

5. **B5** · low · Missing type / long parameter lists
   - **Where:** stills.rs:662-665, 688, 709, 749, 842, 871, 902, 980; the `Interval` literal at 754, 849, 888, 913, 983.
   - **Problem:**
     - `images, mode, policy, config, admission, now` are threaded through 8 methods.
     - `Interval` is built from the approval in 5 places.
     - `policy` and `config` are constant for the owner, yet are passed on every call.
   - **Fix:**
     - Add `Interval::approved(geometry, capture, admission, &FrameApproval)`.
     - Store `&FramePolicy` and `&ResolvedCaptureConfig` inside `Stills<'a>`.
     - Pass `Step<'a> { images, mode, admission, now }` instead of the separate values.
   - **Size:** S–M · **Behaviour:** none

6. **B6** · low · DRY / error idioms
   - **Where:** windows.rs:148-161, 482-487, 737-739, 132-145; mf.rs:102-109; input_helper.rs:418-425; `.map_err(clock_error)` at 64 sites.
   - **Problem:**
     - The 6-field `ProbeError` literal is written 6 times.
     - Every clock conversion needs a manual `.map_err(clock_error)`.
     - `into_error` only forwards to `to_error`.
   - **Fix:**
     - `impl From<ClockRangeError> for ProbeError`, and delete `clock_error`.
     - Add `ProbeError::new(stage)` with setters.
     - Keep one conversion method.
   - **Size:** M (mechanical) · **Behaviour:** none

7. **B7** · low · State as tuples or `Option`s
   - **Where:** windows.rs:250-261, 509-512, 606-626, 684-697, 505-507.
   - **Problem:**
     - A 6-tuple repeats `RecordedVideo`.
     - The input outcome is `Result<Option<(Option<Boundary>, InputPrefix)>>`.
     - `monitor_capture` and `monitor_epochs` are out-parameters that exist only to survive an `Err`.
   - **Fix:**
     - Derive `Default` on `RecordedVideo`.
     - Add `enum InputRun { NotStarted, Cancelled, Closed { cutoff, prefix }, Failed }`.
     - Move the out-parameters into `RecordedVideo`.
   - **Size:** S–M · **Behaviour:** none

8. **B8** · low · DRY
   - **Where:** stills.rs:1377-1388, 1418-1436, 1460-1469 (boundary handlers); quiet reset at 993 and 1069; pop/push-back at 808-820 and 1445-1455.
   - **Problem:** three boundary handlers repeat the same steps and differ only in the drop reasons: drop the `Settling`/`Waiting` slots, drop the scrolls, reset quiet, reset observe. The pending-queue rotation is written twice.
   - **Fix:** add `abandon_unselected(slot_reason, scroll_reason)`, `reset_quiet()`, and `take_decided(..)` for the rotation.
   - **Size:** S · **Behaviour:** none

9. **B9** · low · DRY
   - **Where:** stills.rs:96-100 and 1248-1252; `trigger_rows` at 59-61 and `Trigger::paired` at 272-274.
   - **Problem:** the `ProvisionalInputKind` to `Trigger` mapping is written twice, and the pairing rule is written a third way.
   - **Fix:** `impl From<ProvisionalInputKind> for Trigger`, and make `trigger_rows` use `paired()`.
   - **Size:** S · **Behaviour:** none

10. **B10** · low · Responsibilities
    - **Where:** still_png.rs:182-248, 295-396.
    - **Problem:**
      - The PNG worker is also the owner of the still journal. Its loop interleaves the merge publish, the pause-flush handshake, flush revisions, encoding, counters and health.
      - `save` (~100 lines) mixes the digest, revoked cleanup, deduplication, the rate window, encoding and the authorized publish.
    - **Fix:**
      - Add `still_rows::StillJournal { merge, flushed, revision }` with a `service()` method.
      - Extract `digest()`, `RateWindow` and `write_png()`.
      - The threads stay as they are.
    - **Size:** M · **Behaviour:** none

11. **B11** · low · Inverted dependency / DRY
    - **Where:** input_helper.rs:337-367, 369-427.
    - **Problem:** `runtime_failure` builds a whole `ProbeError` only to read `stage` and `raw_input_registration`. It also derives the native code a second time, with a different mapping.
    - **Fix:** add `HelperError::stage()`, `native_code()` and `raw_input_registration()`, and build both conversions on them.
    - **Size:** S · **Behaviour:** none

12. **B12** · low · Redundant enum state
    - **Where:** preflight.rs:170-192; selector.rs:160, 227, 434, 1095.
    - **Problem:** `Foreground::Ordinary(a)` and `Foreground::Elevated(a)` repeat what `a.elevated.is_some()` already says, so every caller matches `Ordinary(a) | Elevated(a)`.
    - **Fix:** a single `Foreground::Approved(ForegroundApproval)` with an `is_elevated()` method.
    - **Size:** S · **Behaviour:** none (also touches area D)

13. **B13** · low · DRY (approval-guarding)
    - **Where:** preflight.rs:59-60, 200-201; context.rs:1421; context/identity.rs:110; context/elevated.rs:664.
    - **Problem:** `ProcessHandle::open(pid).filter(|p| p.instance(pid) == Some(proof))` appears at 5 sites. If one copy drops the filter, the identity check silently disappears.
    - **Fix:** add `ProcessHandle::open_instance(ProcessInstance)`. The check stays identical.
    - **Size:** S · **Behaviour:** none

14. **B14** · low · DRY
    - **Where:** clipboard.rs:117-122, 170-173; hotkeys.rs:71-77, 394-399; input_helper.rs:81, 247-252; elevated/src/deadline.rs:44.
    - **Problem:** event creation, `OwnedHandle`, `SetEvent` and `MsgWaitForMultipleObjectsEx` are written separately in three owners, each with different error mapping.
    - **Fix:** one `WakeEvent { new(manual), set(), wait_messages(timeout) }`.
    - **Size:** S · **Behaviour:** none

15. **B15** · low · KISS / DRY
    - **Where:** hotkeys.rs:74, 181, 187, 224, 227, 280, 332; fail exits at 346, 360, 364, 384, 397.
    - **Problem:**
      - `CaptureError::owned(…Controller…)` is written out in full 7 times.
      - The pump has 5 identical fail-and-return exits; one of them needs a manual `drop(inbox)`.
    - **Fix:** add `controller_error()`. Make `pump` return `Result<(), PumpFailed>`, and call `fail` once, in the caller.
    - **Size:** S · **Behaviour:** none

16. **B16** · low · Stringly typed paths
    - **Where:** stills.rs:547; still_png.rs:255; package/reader.rs:311, 321.
    - **Problem:** the D-1 rule (still rows reference images relative to `raw/`) is coded by hand in 4 places across 3 modules.
    - **Fix:** add `ArtifactRef::journal_file()` and `ArtifactKind::from_journal_file()` in `package`.
    - **Size:** S · **Behaviour:** none

17. **B17** · low · KISS
    - **Where:** windows.rs:748-754; package/metadata.rs:1114-1127.
    - **Problem:** `windows::replace_manifest` only calls the closure and then `move_file`. Package wraps it again behind cfg and repeats the closure call in the non-Windows branch.
    - **Fix:** call `before_replace()?` once in `publish`, and keep a cfg'd `rename_replace`.
    - **Size:** S · **Behaviour:** none

## Assessment
The contracts hold, the round-1 fixes are in place, there are no TODOs, and the `unsafe` blocks state their invariants. clipboard, hotkeys, indicator, os and the helper launch are small and clear.

The debt is structural:
- the god module `stills.rs`, which also threads its parameters through every call (B1, B5);
- the untyped, string-keyed still row, plus drop reasons that live outside the enum (B2, B3);
- controller and package policy inside `record_with_disk`/`record` (B4).

These are fine as they are: `still_time`, `os.rs`, `clipboard.rs`, `ui_window`, and the `reclaim` grouping.
