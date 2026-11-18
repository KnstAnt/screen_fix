# Findings, area E: package and capture model (round 2)

Scope:
- `core/src/package.rs`
- `core/src/package/**`
- `core/src/capture/mod.rs`
- `core/src/capture/**`

Round-1 items E1–E10 are already applied.

Counts: high 0, medium 6, low 9. Every finding has behaviour `none`. E6 only adds a test.

1. **E1** · medium · Layering (dependency cycle)
   - **Where:** metadata.rs:481, 504-506, 728, 804, 902, 989-990; diagnostics.rs:68, 76; reader.rs:88-89; capture/input/control.rs:5, 45; session.rs:43, 69, 100, 120-165.
   - **Problem:**
     - `package` imports controller types from `session`: `PauseReason`, `AdmissionBoundary`, `DiskPreflight`, `SourceFailure`, `SourceFailureReason` and `SourceStage`.
     - `session` imports `package` back (for example `DegradedActivity` uses `FailureComponent`), so the two modules depend on each other.
     - The serialized manifest vocabulary sits next to UI guidance text (`SourceFailure::guidance`).
     - `capture::input::control` depends on `session::hotkey::HotkeyActivation`, but only reads `virtual_key` and `message_tick`.
   - **Fix:**
     - Move these types into a new `package/source.rs` (or into `protocol`), and add `pub use` re-exports in `session`.
     - Make `guidance()` a free function in `session`.
     - Change `ControlChords::confirm` to take `ControlNotice { virtual_key, message_tick }`.
   - **Size:** M · **Behaviour:** none

2. **E2** · medium · Stringly typed
   - **Where:** journal.rs:278-314; reader.rs:219-347, 402-410, 442-498. The blind row is built 3 times: journal.rs:986-988, input/blind.rs:23, input.rs:1396.
   - **Problem:**
     - Rows are untyped `serde_json::Value`.
     - The writer parses the `kind` string again to tally a row.
     - The reader matches the same strings in 3 constant lists (`ACTIONS`, `PAUSE_CONTEXT`, `PAUSE_INPUT`) and 2 `match` blocks.
     - About 52 `"kind"` literals must be kept in sync by hand. A typo silently turns into `RowTally::Plain` or a gate-4 violation.
   - **Fix:**
     - Add `package/row.rs` with `enum RowKind` (`snake_case`, same names) and the predicates `is_action()`, `allowed_in_pause(StreamId)` and `tally()`.
     - Parse `kind` once on each side.
     - Add `InputBlind::row()`.
     - Producers can be moved over later.
   - **Size:** M (L if all producers move) · **Behaviour:** none

3. **E3** · medium · DRY
   - **Where:** journal.rs:260-326, 960-1010; reader.rs:52-78, 216-235, 341-346.
   - **Problem:**
     - The writer (`RowTally`/`data`/`gap`) and the reader (`RowTallies` plus inline arms) each implement the same counting rules for pause, blind, suppressed, unavailable and gap rows.
     - `gap_items`/`unknown_gaps` is copied line for line.
     - The doc says the two sides count the same rows, but nothing enforces it.
   - **Fix:**
     - Add `struct RowCounts` with `count_row(&RowTally)` and `count_gap(&GapRecord)`, used by both sides.
     - Add a projection `RowCounts::of(&StreamSummary)`. The serialized summary stays the same.
     - `reconciles` then becomes `==` plus the outcome total.
   - **Size:** M · **Behaviour:** none

4. **E4** · medium · God function / KISS
   - **Where:** reader.rs:167-367, 185, 260, 361-362, 417-426.
   - **Problem:**
     - `validate_gates` is about 200 lines of nested loops, with inline checks for frames (40 lines), stills (35) and cursors (15).
     - Segment progress is an anonymous 4-tuple, destructured 3 times.
     - The pause intervals and findings in `Gates` are tuples.
   - **Fix:**
     - Add `JournalScan` with `check_frame_row`, `check_still_row` and `check_cursor_asset`.
     - Add `struct SegmentProgress { summary, frames, last_pts, end_100ns }`.
     - Add `PauseInterval` and `Finding`.
   - **Size:** M · **Behaviour:** none

5. **E5** · medium · Module with several concepts
   - **Where:** metadata.rs:1-864 (DTOs), 30-106 (errors), 866-1127 (owner), 1129-1233 (reader); metadata.rs:276.
   - **Problem:**
     - The 1233-line `metadata.rs` mixes the manifest schema, the error type, the live publisher and half of `PackageReader`. The other half of `PackageReader` is in `reader.rs`.
     - The DTO `MonitorVideo` also holds live state: `open_interval: Option<(AdmissionEpoch, SessionTimeUs, bool, bool)>`.
   - **Fix:**
     - Split into `package/manifest.rs`, `package/error.rs` and `package/owner.rs` (`MetadataOwner`, `AttemptLayout`, `AttemptFaults`).
     - Move the `PackageReader` parts into `reader.rs`.
     - Add `struct OpenInterval { epoch, start, has_video, long }`.
   - **Size:** M (mostly moves) · **Behaviour:** none

6. **E6** · medium · Debt
   - **Where:** metadata.rs:1129-1223; reader.rs:104-367; package.rs:11.
   - **Problem:**
     - `PackageReader::open`, `validate_gates`, `source_summary` and `SourceSummary` (about 650 lines, specified in the schema doc) have no caller in the workspace: no UI path, no binary, no test.
     - The crate has no `#[cfg(test)]` at all.
     - The gates are therefore never run, and nothing catches drift between the writer and the reader.
   - **Fix:**
     - Add one integration test, `core/tests/package_gates.rs`.
     - It writes a minimal synthetic attempt through `JournalOwner`/`MetadataOwner`, or uses a fixture.
     - It asserts that `validate_gates` passes, and that it fails once a row is corrupted.
     - A "verify recording" command in the app would be a `change`.
   - **Size:** M · **Behaviour:** none (test only)

7. **E7** · low · Near-duplicate types
   - **Where:** outcome.rs:224-291; journal.rs:228-256; matching.rs:97-135.
   - **Problem:**
     - `LossEvidence` (Ordinary), `PendingLoss` and `InputGap` all carry reason, producer, count and range.
     - `InputGap` stores the range as 4 loose fields and rebuilds a `LossRange` on every merge.
   - **Fix:**
     - Add `struct OrdinaryLoss { reason, producer, count, range: LossRange }` with `merge` and `record()`.
     - Give `InputGap` a `range: LossRange`.
   - **Size:** S–M · **Behaviour:** none

8. **E8** · low · DRY (overlaps C9)
   - **Where:** held.rs:181-189 / 343-353, 242-243, 251, 420-431; normalize.rs:282-292; resync.rs:14-16, 22; text.rs:180-183; receiver.rs:1178-1183.
   - **Problem:**
     - The list of 8 modifiers is written inline 4 times, and the button list twice, although `ALL` exists.
     - The VK↔modifier table exists in both directions, in 2 files.
     - The mask computation is duplicated.
   - **Fix:**
     - Add `const MODIFIER_VKS: [(Modifier, u16); 8]` with `Modifier::vk()` and `from_vk()`.
     - Use `ALL` at the inline sites.
     - Add a `masks(iter)` helper.
   - **Size:** S · **Behaviour:** none

9. **E9** · low · Hand-written combinators
   - **Where:** protocol/src/evidence.rs:17-21; capture/input/evidence.rs:80-141; held.rs:150-153, 395-400.
   - **Problem:**
     - `Evidence<T>` has no `map` and no `observed()`.
     - The workspace hand-writes `map_or(Evidence::Unavailable(..), Evidence::Observed)` 15 times and `Observed(x) => Some(x)` 5 times.
   - **Fix:**
     - Add `Evidence::map`, `observed()` and `from_option(opt, reason)`.
     - Delete `held::observed_device`.
   - **Size:** S · **Behaviour:** none

10. **E10** · low · Small helpers (overlaps D10)
    - **Where:**
      - `input.normalized.correlated.first` appears 19 times.
      - `Some(x.map_or(v, |o| o.max(v)))` appears 8 times: control.rs:107, 111; matching.rs:220, 389, 457; receiver.rs:976, 1366; stills.rs:976.
    - **Fix:** Add `HeldInput::first()`, `HeldInput::receipt()` and a generic `raise(slot, value)`.
    - **Size:** S · **Behaviour:** none

11. **E11** · low · Mixed responsibilities
    - **Where:** matching.rs:184-200, 230-240, 277-296, 387-396, 733-823 (pen); held.rs:104-133, 204-206, 234-236 (AltGr).
    - **Problem:**
      - `InputMatcher` mixes raw/hook matching with pen-promotion association: 4 fields and about 110 lines.
      - `PhysicalHeld` keeps AltGr state in 3 loose fields and resets them in 3 places.
    - **Fix:**
      - Add `PenAssociation` (`forget_links`, `forget_device`, `note_uncertain`, `link`).
      - Add `AltGrDetector` (`observe`, `reset`, `active`).
    - **Size:** M · **Behaviour:** none

12. **E12** · low · Dead or speculative API
    - **Where:** capture/save.rs:12-16, 25-28, 35-37, 92-102, 134-136; input.rs:1609; hotkey.rs:73-76.
    - **Problem:**
      - `ConfirmationSource` has a single variant.
      - `SaveEvidence.request` is always `None`.
      - `ConfirmedSave::{routing, evidence, attempt}` and `SaveObservation::attempt` have no caller.
      - `CheckpointRequest` only wraps an id.
      - The module is `pub`, so the compiler does not warn about any of this.
    - **Fix:** Remove the unused items. Keep `pub` only on what the UI uses (`FixtureSave`, `SaveKind`, `FixtureSaveError`, `FixtureSaveResult`), and make everything else `pub(crate)`.
    - **Size:** S · **Behaviour:** none

13. **E13** · low · KISS / DRY
    - **Where:** journal.rs:437-443, 452-461, 556-566, 651-655, 667-668, 690-695, 730-737.
    - **Problem:** Queueing an entry is written 5 different ways, each with its own mapping of `Full`/`Disconnected`.
    - **Fix:** Add `fn queue(&self, entry, room: Capacity) -> Result<bool, JournalError>` and `Capacity::Unreserved` for frames.
    - **Size:** S · **Behaviour:** none

14. **E14** · low · Magic numbers / misplaced constants
    - **Where:** normalize.rs:156, 189, 227-252, 229/249, 261-279, 298-315; matching.rs:935, 964-969.
    - **Problem:**
      - `normalize.rs` is documented as the place that interprets Windows input wire values, yet most of those values are bare literals.
      - The pen/touch promotion constants live in `matching.rs`.
    - **Fix:**
      - Name the constants: `RI_KEY_BREAK`, `RI_KEY_E0`, `RI_KEY_E1`, `MOUSE_MOVE_ABSOLUTE`, `WM_*`, `VK_*`, `ALTGR_COMPANION_SCAN`.
      - Move `promoted()` and the promotion constants to `normalize.rs`.
    - **Size:** S · **Behaviour:** none

15. **E15** · low · DRY / idiomatic
    - **Where:** package.rs:65-98; metadata.rs:948-953, 816, 912; reader.rs:311, 321.
    - **Problem:**
      - The attempt layout strings are spelled out in 3 files.
      - `streams.len() != 4` duplicates `STREAMS`.
      - An `AttemptId::new` failure on the write path is reported as `InvalidManifest`, which is misleading.
    - **Fix:**
      - Add `fn journal_file(StreamId)`, the constants `RAW_VIDEO`/`RAW_IMAGES`/`RAW_CURSORS` and `ArtifactKind::video_dir`.
      - Use `STREAMS.len()`.
      - Map the id failure to `PackageError::io(CreateAttempt, ..)`.
    - **Size:** S · **Behaviour:** none

## Assessment
Overall health is fair:
- The types are precise and invariants are documented.
- There are no `unwrap` calls on fallible paths.
- The round-1 fixes have been applied.

Top debts:
1. Untyped row strings, and counting rules duplicated between the writer and a reader that nothing calls (E2, E3, E6).
2. The `session`⇄`package` dependency cycle (E1).
3. The oversized `metadata.rs` and `validate_gates` (E4, E5).

Fine as is:
- the `outcome.rs` vocabulary;
- the journal design (ingress, ledger, reserve, tail);
- the `capture::{frame, context, observation}` types;
- the staged input pipeline;
- `control.rs` and `sampling.rs`.
