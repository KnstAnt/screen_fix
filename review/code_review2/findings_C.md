# Findings, area C: input platform (`platform/windows/input.rs`, `platform/windows/input/**`)

The following files were read in full:
- `input.rs` (1797 lines)
- `receiver.rs` (1427)
- `gate.rs` (1430)
- `hid.rs`, `touchpad.rs`
- `text.rs`, `blind.rs`, `devices.rs`
- `hook_health.rs`, `resync.rs`

Round 1 fixes are confirmed and not repeated here.

Counts: high 2, medium 5, low 11. Every finding has behaviour `none`. C15 is a `change` only if its policy is unified.

1. **C1** · high · Architecture
   - **Where:** input.rs:247-273 (`Pump`, 25 fields), 278-477 (`admitted`), 482-670 (`pause`), 674-827 (`stop`), 920-1163 (`poll_elevated`, 13 params, 245 lines).
   - **Problem:**
     - `Pump` has 25 fields, and their drop order depends on the field order.
     - Every method destructures all of them through `ref mut`, so the split into methods is in name only.
     - `admitted()` is a 190-line closure called on the spot.
     - `pause()` contains 5 hand-written poll loops.
   - **Fix:**
     - Group the fields into sub-structs:
       - `PumpEnv<'a>` (Copy references)
       - `ContextStream`/`InputStream` (C2)
       - `HelperSlot` (C13)
       - `HealthCursor`
     - Replace the closure with `fn admit_once(&mut self) -> Result<ControlFlow<..>>`, split into `service_starting`, `service_ordinary`, `service_elevated` and `publish_tick`.
     - Move the elevated part to `input/elevated_lane.rs` (`ElevatedLane`).
     - Move `ElevatedSaves` and `route_fixture` to `input/saves.rs`.
   - **Size:** L · **Behaviour:** none

2. **C2** · high · Missing type
   - **Where:**
     - Context: input.rs:353-1783 (~30 sites).
     - Input: gate.rs:328-375, 512-516, 727-731, 1129-1275; receiver.rs:751-761; input.rs:1391-1399.
   - **Problem:**
     - The context stream travels as a triple `(context, context_prefix, next_context_index)`, and the input stream as a pair `(output, prefix)`.
     - The rule "submit, then advance the prefix" is repeated at each site:
       - `prefix.advance_time(at, output.try_submit(..)..?)` appears 6 times;
       - the index increment is duplicated;
       - `prefix.last_time.unwrap_or(receipt.boundary.time)` appears 7 times.
     - `input_timing(.., None)` is called 16 times, always with `None`.
   - **Fix:**
     - Add `ContextWriter{journal, prefix, next_index}` with `timing`, `event`, `boundary`, `submit_health`, `record_loss` and `flush`.
     - Add `InputWriter{journal, prefix}` with `submit`, `submit_boundary`, `record_loss`, `submit_health` and `published_or(boundary)`.
     - Make `ReceiptPrefix` private to the writers, and fold `DeferredFlush` into them.
   - **Size:** M · **Behaviour:** none
   - **Order:** do this first; it shortens the parameter lists for C1 and C3.

3. **C3** · medium · Architecture
   - **Where:** gate.rs:37-62 (22 fields), 727-1098 (`publish`, ~370 lines), 512-725 (`drain`, ~215 lines).
   - **Problem:**
     - `Gate` mixes 8 concerns: the slot queue, modifiers, HID, touchpads, pen, redaction, motion and text.
     - `publish` builds four kinds of rows inline, patches ~35 envelope keys, and has 5 consecutive `if disposition == Submit::Queued` blocks.
     - `drain` mixes text, the approval of the front slot, dispatch and redaction.
     - A privacy change therefore means editing inside 370- and 215-line functions.
   - **Fix:**
     - Extract `Redactions` (with a named cap), `TouchpadInventory` and `PenEvidence`.
     - Add row builders `key_row`, `button_row`, `motion_row` and `hid_row` that return `RowDraft`.
     - Add `decorate()` and `after_submit()` with a single match.
     - Add `drain_confirmed_text` and `decide_front() -> Front{Wait, Publish{..}}`.
   - **Size:** L · **Behaviour:** none

4. **C4** · medium · Layering
   - **Where:**
     - gate.rs:64-99, 109-162, 207-244, 1364-1390
     - receiver.rs:1071-1134
     - blind.rs, hook_health.rs
     - input.rs:864-889 (`pen_capture`), 1165-1201 (tail-reason priority), 1401-1436 (`pause_blind_labels`)
   - **Problem:**
     - Addendum §4 places the gate (sensitive classification, held state) in `capture::input`.
     - Instead, pure policy sits under `platform/windows`, with no Win32 and no `Context`: `ModifierKnowledge`, `Outcomes`, `MotionCounts`, `record_loss`/`is_key`, `ProducerLoss`/`SampleLoss`, `Blinds` and `HookHealth`.
     - There are no `cfg(test)` tests.
   - **Fix:**
     - Move these to `capture/input/{publication, loss, blind, hook_health}.rs`.
     - Move the tail priority, `pen_capture` and `pause_blind_labels` next to `pause_blind_reason` in `session`.
     - Leave in the gate only the code that queries `Context`.
   - **Size:** M · **Behaviour:** none

5. **C5** · medium · State machine in booleans and Options
   - **Where:** receiver.rs:59-95, 356, 602-606, 619-656, 716-725, 780-797, 972-977, 1195-1427.
   - **Problem:**
     - `Receiver` has ~30 fields.
     - Resync is spread over 6 fields (`opened_at`, `sync_after`, `last_sync_attempt`, `requires_opening_sync`, `synchronized_qpc`, `sync_visit`), changed by 7 methods whose invariants disagree. `opened_at` is only checked with `is_none()`.
     - `reopen()` resets 15 fields by hand.
     - `ProvisionalRequests` (230 lines) is a separate concept.
   - **Fix:**
     - Add a `Resync` type with `request`, `install`, `reopen`, `due` and `blocks`.
     - Add `Liveness` and `DeviceInventory`.
     - Move `ProvisionalRequests` to `input/provisional.rs`.
   - **Size:** M · **Behaviour:** none
   - **Note (PLAUSIBLE):** the invariant gaps may be harmless, because the elevated path does not call `try_synchronize`.

6. **C6** · medium · Architecture / KISS
   - **Where:** input.rs:674-827, 318-334.
   - **Problem:**
     - `stop` decides its mode from `cutoff`, `paused`, `elevated_epoch` and `decides_stop`, and re-checks these 9 times.
     - It calls `foreground_closing_receipt(cutoff)?` 6 times.
     - `admitted` branches on combinations of the booleans `elevated`, `ordinary_live` and `latched`.
   - **Fix:**
     - Compute once `enum Closing{NoAdmission, WhilePaused{..}, Ordinary{cutoff, receipt}, Elevated{cutoff, receipt}}`.
     - Add `enum Lane{Elevated, OrdinaryLive, Latched, Inactive}`.
   - **Size:** M · **Behaviour:** none
   - **Note (PLAUSIBLE):** computing the receipt once is safe if `tick_anchor` does not change after the cutoff (see session/windows.rs:1023-1034).

7. **C7** · low · DRY
   - **Where:** input.rs:959, 978-983; receiver.rs:416-419.
   - **Problem:** the mapping from `GapCause` to `LossReason` is written 3 times.
   - **Fix:** `impl From<GapCause> for LossReason` in `package::outcome`.
   - **Size:** S · **Behaviour:** none

8. **C8** · low · DRY
   - **Where:** gate.rs:1286, 1320; receiver.rs:1319, 1416, 1425; text.rs:289; capture/input control.rs:300, evidence.rs:54, 155, matching.rs:74, 86.
   - **Problem:** `once(&c.first).chain(&c.partners)` appears 11 times.
   - **Fix:** add `CorrelatedInput::slots()`, and build `records()` on top of it.
   - **Size:** S · **Behaviour:** none

9. **C9** · low · DRY
   - **Where:** resync.rs:14-24; text.rs:180-183 (also held.rs:242).
   - **Problem:** the modifier and button lists are written out by hand, even though `Modifier::ALL` and `Button::ALL` exist. `from_snapshot` zips them by position, so reordering a list silently mislabels the values.
   - **Fix:** use `Modifier::ALL` and `Button::ALL`.
   - **Size:** S · **Behaviour:** none

10. **C10** · low · DRY
    - **Where:** blind.rs:21-29; input.rs:1395-1397; package/journal.rs:986-988.
    - **Problem:** the `blind` row is built from string keys in 3 places, although a typed `InputBlind` already exists (metadata.rs:416).
    - **Fix:** a single serde `BlindRow` in `package`, with `producers` skipped when empty. The names stay the same.
    - **Size:** S · **Behaviour:** none

11. **C11** · medium · Stringly typed
    - **Where:** gate.rs:849-860, 940-1022, 1394-1430; hid.rs:139-200; input.rs:578-588, 644-659, 797-803, 986-989, 1449-1453, 1555-1571, 1653-1678 (122 `json!` calls).
    - **Problem:** the fixed schema is produced by `json!` literals that are then patched with `row["key"] = ...`. Checkpoint reasons are `&str`. A key typo or a lost key compiles.
    - **Fix:** use typed serde rows with the same names:
      - `InputEnvelope` with a flattened payload
      - `HeldRow`
      - `Pause`/`Resume`/`Discontinuity`/`InputSourceLoss`/`CheckpointContext`
      - `enum CheckpointUnavailable`
    - **Size:** L · **Behaviour:** none

12. **C12** · low · KISS
    - **Where:**
      - "fail source": input.rs:492-494, 932-943, 1076-1080, 1088-1091, 1284-1287
      - "leave visit": 1043-1046, 1068-1074, 1109-1118, 1127-1130
    - **Problem:**
      - `frames.clear(); fail_source(..); cancel()` appears 6 times.
      - The "leave visit" sequence appears 4 times.
      - `current_visit() && elevated_active(epoch)` is checked 8 times.
    - **Fix:** add helpers `fail_source`, `visit_live` and `leave_visit`, and compute `certified` once.
    - **Size:** S · **Behaviour:** none

13. **C13** · low · DRY / KISS
    - **Where:** input.rs:298-306, 337-342, 384-386, 716, 1299-1305, 1292-1327, 498-520, 569-576, 604-633, 553-557/593-597.
    - **Problem:**
      - `notify(take_input_environment()).and_then(poll)` appears 6 times.
      - `helper` and `retired_helper` always go together.
      - `pause` has 5 near-identical "tick, poll, park" loops.
    - **Fix:** add `HelperSlot{active, retired}` with `poll_with_env`, `retire` and `is_live`, plus a single `wait_paused(until, done)`.
    - **Size:** M · **Behaviour:** none

14. **C14** · low · DRY
    - **Where:** input.rs:1336-1354, 1356-1378.
    - **Problem:** `DeferredFlush::poll` and `flush_boundary` each implement the three-way `FlushOutcome::try_recv`.
    - **Fix:** a shared `poll_flush()`.
    - **Size:** S · **Behaviour:** none

15. **C15** · low · DRY / KISS
    - **Where:** gate.rs:378-383, 390-392, 438-442.
    - **Problem:** the overflow guard is copied 3 times. Two copies update `overflow_at` with `get_or_insert` and one with `min`, and nothing explains the difference.
    - **Fix:** add `staging_full(at, policy)`. Unifying the copies on `min` is a `change`.
    - **Size:** S · **Behaviour:** none

16. **C16** · low · DRY
    - **Where:** hook_health.rs:31-36, 59-65, 124-130; devices.rs:25-40.
    - **Problem:** the match from `Producer` to `Channel` is written 3 times, although `channel()` exists. The elevated/ordinary inventory choice is written 3 times.
    - **Fix:** use `channel()`, and add `Devices::inventory(elevated)`.
    - **Size:** S · **Behaviour:** none

17. **C17** · low · Idiomatic
    - **Where:** receiver.rs:79, 592, 873-880; gate.rs:477; input.rs:815, 870.
    - **Problem:** these are positional tuples:
      - `(bool, bool)` for hook loss and for pen observation;
      - `[Option<Producer>; 2]`;
      - the 4-tuple returned by `take_loss`.
    - **Fix:** add `HookLoss`, `PenObservation` and `ElevatedLoss`, and make `take_hook_reports` return an iterator.
    - **Size:** S · **Behaviour:** none

18. **C18** · low · Idiomatic / debt
    - **Where:**
      - magic numbers: gate.rs:828 (500_000), 896 (300_000), 1162/254 (64); hid.rs:80 (12_000); touchpad.rs:42 (100_000)
      - bare VK codes: gate.rs:157-159, text.rs:141
      - no SAFETY comment: receiver.rs:1138, 1187; text.rs:195, 300; gate.rs:1358
      - cast: text.rs:193
    - **Problem:**
      - Magic numbers.
      - Bare VK codes, and a bare `ToUnicodeEx` flag 0x5.
      - 5 unsafe FFI calls with no SAFETY comment.
      - `layout as usize as HKL`, while receiver.rs:1137 uses `try_from`.
    - **Fix:** use named constants and the `windows_sys` `VK_*` constants, add SAFETY comments, and add one `hkl(layout)` helper.
    - **Size:** S · **Behaviour:** none

## Assessment
The code looks correct and is well commented. The leaf modules (blind, devices, hid, touchpad, text, resync) are fine, apart from the small points C9, C16 and C18.

Main debts:
- god objects and god functions: the pump in `input.rs`, and `Gate::publish`/`drain` (C1, C3);
- missing writer types, which spread the stream update rules across ~40 sites (C2);
- pure policy sitting in `platform/windows` without tests (C4), and hand-built JSON rows (C11).

Order: start with C2.
