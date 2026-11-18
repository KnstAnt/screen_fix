# Findings, area F: session, protocol, UI (round 2)

Scope:
- `core/src/session/**`
- `core/src/{config,devlog,error,lib,session}.rs`
- `protocol/**`
- `ui/**`

Round-1 items F1–F17 were checked; those already applied are not repeated.

Counts: high 1, medium 5, low 13. Every finding has behaviour `none`. None changes the serialized format.

1. **F1** · high · Architecture
   - **Where:** session/windows.rs:1-2231. `Facts` is at 215-263; `impl WorkerSession` at 512-1189 (89 `pub(crate)` fns); the controller at 1198-2214.
   - **Problem:** One module holds six concepts:
     - the lifecycle state;
     - the worker facade;
     - the controller loop;
     - the startup countdown and admission check;
     - the elevated source;
     - the pause/resume/stop transitions.

     `Facts` has about 40 flat fields, grouped only by comments. Every new fact widens both `Facts` and `WorkerSession`.
   - **Fix:**
     - Split the module into `windows/{lifecycle, worker, startup, source, transitions}.rs`.
     - Group `Facts` into four sub-structs:
       - `PauseFacts`: pause_prefix, paused_video, pause_interval_reasons, last_pause, input_prefix, pause_started, suspend_wait, suspend_cleanup;
       - `SourceFacts`: source_epoch, elevated_epoch, source_change, source_preparation, source_ready, startup_target, source_evidence, selected_intervals;
       - `ClockFacts`: utc_anchors, tick_anchor, clock_failed;
       - `Reports`: video_failures, health, omitted_health, input_failures, faults.
     - A pause reset becomes `facts.pause = PauseFacts::default()`, instead of the assignments at 2140-2145 and 2181-2185.
   - **Size:** L · **Behaviour:** none

2. **F2** · medium · Architecture
   - **Where:** windows.rs:1418-1495 (`coordinate`, 10 params); 1520, 1598, 1633, 1813, 1832, 1856; 2109-2123, 2117.
   - **Problem:**
     - The loop locals (countdown, reasons, pause_finished, disk_resume, subscribed) are threaded through 8 free functions.
     - `reasons` is mirrored into `facts.status.pause_reasons`, so it has two sources of truth.
     - In `pause_command(SessionCommand)` the `Stop => false` arm is dead.
     - At 15 sites a `Pause`/`Resume` command is built only to be destructured again.
   - **Fix:**
     - Add `struct Controller<'a>{lifecycle, session, hotkeys, environment, frequency, countdown, pause_finished, disk_resume, subscribed}` and turn the free functions into its methods.
     - Keep the reasons only in `Facts` (as a BTreeSet) and derive the status from them.
     - Replace `pause_command` with `set_pause(reason, active)`.
   - **Size:** M · **Behaviour:** none

3. **F3** · medium · DRY (workspace)
   - **Where:** windows.rs:1648-1659, 2202-2209; environment.rs:101-119; elevated/visit.rs:593-597; elevated/peer.rs:46-47; elevated runtime/lifetime.rs:36-40; runtime/context.rs:743; platform/windows/clock.rs:52-54, 80-82.
   - **Problem:** The QPC↔Duration conversion is written by hand about 8 times, with different rounding, overflow handling and intermediate types. `QpcFrequency` has no methods at all.
   - **Fix:** Add `QpcFrequency::{duration(u64)->Duration, ticks(Duration)->Option<i64>, ticks_ceil}` and `QpcTick::ticks_since`. Keep each call site's rounding as it is.
   - **Size:** M · **Behaviour:** none

4. **F4** · medium · DRY
   - **Where:** protocol/src/ipc/elevated.rs:243-254 vs protocol/src/ipc.rs:579-592; conversions at elevated runtime/input.rs:150-154, 207-222 and receiver.rs:258-267; accessors at elevated/ingress.rs:266-301.
   - **Problem:** `InputEvidence` repeats 7 `HelperToCore` variants. The two are converted by hand, in both directions, in two crates. A new record kind means editing 2 enums, 2 conversions and 4 matches.
   - **Fix:** Keep both wire enums. Add `From<InputEvidence> for HelperToCore`, `TryFrom<HelperToCore> for InputEvidence` and `InputEvidence::{producer, first_qpc, last_qpc}`.
   - **Size:** M · **Behaviour:** none

5. **F5** · medium · Architecture
   - **Where:** core/src/session.rs:323-382; uses of `facts.status.{clock,opened,closed,stop}` throughout windows.rs; ui_window/indicator.rs:131-150; ui/src/app.rs:466-532.
   - **Problem:**
     - `SessionStatus` serves both as the UI snapshot and as the controller's authoritative state. Its public fields clock, opened, closed and stop are never read by the UI.
     - Each `status()` call clones the whole struct, including its Vecs, and the indicator calls it on every refresh.
     - The UI and the indicator each derive their labels with their own match ladders.
   - **Fix:**
     - Move the boundaries into a private `Boundaries` struct inside `Facts`, and make `SessionStatus` a view.
     - Add `SessionStatus::summary() -> StatusSummary`.
   - **Size:** M · **Behaviour:** none

6. **F6** · medium · Architecture
   - **Where:** ui/src/app.rs:23-116 (27 fields, 18 cfg attributes, a hand-written `Default`); 309-700 (`recording_panel`, about 390 lines).
   - **Problem:** One struct mixes the setup form, hotkeys, the indicator, the session state machine and fixture save. One function renders all of it.
   - **Fix:**
     - Split the state into `#[derive(Default)]` sub-structs `Setup`, `HotkeyState` and `Indicator`.
     - Split the panel into `setup_panel`, `status_panel`, `session_controls` and `acknowledge_panel`.
   - **Size:** L · **Behaviour:** none

7. **F7** · low · Architecture
   - **Where:** windows.rs:286-300, 1258-1261; environment.rs:206-241 (6-param `spawn`), 218-229, 461-462, 479-480, 60-64, 267-270.
   - **Problem:**
     - `Lifecycle` has five `Arc` fields that exist only to be copied into `Signals`.
     - The "publish to both bitsets" step is written 4 times.
     - Pause reasons cross module boundaries as `(u32, u32)` masks.
   - **Fix:**
     - Add `struct EnvironmentSignals{input, elevated, display_revision, session_end, power}` with `publish(bits)`, held in a single `Arc`.
     - Make `reasons()` return an iterator of `(PauseReason, asserted, active)`.
   - **Size:** M · **Behaviour:** none

8. **F8** · low · DRY
   - **Where:** session.rs:118-154 and 221-268 (a 32-arm map); elevated/src/win.rs:12-87.
   - **Problem:** `elevated::Failure` and `SourceFailureReason` map 1:1, so a new failure needs 4 edits. The `cfg(windows)` mapping also sits in the portable `session.rs`.
   - **Fix:** Add `Failure::reason()` next to the enum, which means moving `SourceFailureReason` into protocol. At minimum, move the mapping into `session/windows`.
   - **Size:** M · **Behaviour:** none

9. **F9** · low · DRY
   - **Where:** protocol attachment.rs:134-155 vs runtime.rs:455-513 (and ipc.rs:695-751).
   - **Problem:** The 40-byte frame header is written twice with the same hard-coded offsets. Only the magic differs.
   - **Fix:** Add `FrameHeader{magic, version, role, kind, direction, length, sequence, connection}` with `write`/`parse`. The wire format stays the same.
   - **Size:** M · **Behaviour:** none

10. **F10** · low · KISS (dead weight)
    - **Where:** config.rs:121-145, 166-167, 222-224, 80; platform/mod.rs:90-96; ui/app.rs:921-928.
    - **Problem:** `EffectiveCapabilities` and `VideoMode` are never read. Their only use is to reject anything other than `awaiting_probe()`.
    - **Fix:** Remove them until a milestone needs them, and keep the calibration check.
    - **Size:** S · **Behaviour:** none

11. **F11** · low · DRY
    - **Where:** windows.rs. Push-or-truncate appears at 613-617, 632-643, 669-673, 1941-1945 and 2052-2058; `let _ = wake.try_send(())` ×13; `clock::qpc().map_err(controller_error)?` ×10; `utc_anchor().map_err(controller_error)?` ×5.
    - **Fix:** Add `push_bounded<T>(&mut Vec<T>, T, max) -> bool`, `WorkerSession::wake()`, `controller_qpc()` and `controller_utc_anchor()`.
    - **Size:** S · **Behaviour:** none

12. **F12** · low · Idiomatic
    - **Where:** windows.rs:1336-1342, 1386-1395 (`Option<Option<WorkerOutcome>>`); session.rs:353-382 (a hand-written `Default`); windows.rs:1138-1145; windows.rs:1655, 1659 (`as u8`, `as u64`).
    - **Fix:**
      - Add `enum WorkerState{Running, Exited(Option<_>)}`.
      - Derive `Default`, with `#[default]` on the starting variants.
      - Use `mem::replace`.
      - Use `try_from` instead of the casts.
    - **Size:** S · **Behaviour:** none

13. **F13** · low · DRY
    - **Where:** error.rs:44-114 (a 7-field literal ×5); session/fault.rs:37-82.
    - **Problem:** `FaultReport` copies kind, failure, message and remedy, then rebuilds the `CaptureError`. Last round's `remedy` field had to be threaded through 6 places.
    - **Fix:** Add a private `CaptureError::new`, plus a Clone `CaptureCause{kind, failure, message, remedy}` that both types store.
    - **Size:** S · **Behaviour:** none

14. **F14** · low · DRY
    - **Where:** ui/app.rs.
    - **Problem:**
      - The indicator-failure block appears 3 times (144-149, 296-300, 941-945), plus a variant at 152-158.
      - Pause labels are built twice (468-473, 516-523), and the coverage text twice (414-423, 563-569).
      - The "is the selection connected" predicate appears 4 times.
      - `with_output_root(self.folder.clone())` appears 9 times.
      - `recording_error(.., Option<&Context>)` is always called with `Some`.
      - `display_ids.len() as u32` (808), and `display_ids` is keyed by `String`.
    - **Fix:**
      - Add `fail_indicator`, `pause_label`, `coverage_lines`, `is_connected` and `snapshot`.
      - Make the parameter a plain `&Context`.
      - Key the map by `DisplayKey`.
      - Use `u32::try_from` instead of the cast.
    - **Size:** S · **Behaviour:** none

15. **F15** · low · Layering
    - **Where:** ui/app.rs:183-201.
    - **Problem:** The first arm of `match error.source_failure()` ignores the scrutinee. The admitted / not-started / elevated classification is domain logic sitting in the UI.
    - **Fix:** Add `CaptureError::attempt_stage() -> AttemptStage` in core.
    - **Size:** S · **Behaviour:** none

16. **F16** · low · Layering
    - **Where:** protocol/src/ipc.rs:340-366 (`pen_continuity_uncertain`, `pen_tip`).
    - **Problem:** These are Recorder interpretation rules, not wire validation.
    - **Fix:** Move them to `capture/input/pen.rs`.
    - **Size:** S · **Behaviour:** none

17. **F17** · low · Idiomatic
    - **Where:** protocol runtime.rs:236-240, 491.
    - **Problem:** Every encode clones two `Box<str>` ids.
    - **Fix:** Use a borrowed `ScopedRef<'a, T>`.
    - **Size:** S · **Behaviour:** none

18. **F18** · low · Debt / conventions
    - **Where:** session.rs:384-430.
    - **Problem:** Every `SessionHandle` method carries a `cfg(windows)` / `cfg(not(windows))` pair.
    - **Fix:** Add a `cfg(not(windows))` stub `Control` with the same API.
    - **Size:** S · **Behaviour:** none

19. **F19** · low · DRY
    - **Where:** config.rs:87-91, 185-191, 193-197.
    - **Problem:** An O(n²) duplicate search is written 3 times.
    - **Fix:** Add `has_duplicate(items, same)`.
    - **Size:** S · **Behaviour:** none

## Assessment
The area is correct and well commented. The two central modules, however, are god objects:
- `session/windows.rs` (2231 lines, about 40 `Facts` fields, 89 `WorkerSession` methods);
- `ui/src/app.rs` (27 fields, a 390-line panel).

The top debts are:
- the controller split (F1/F2);
- a single QPC↔Duration helper (F3);
- the mirrored input enums and `SessionStatus` as both state and view (F4/F5).

Fine as is: `fault.rs`, `hotkey.rs`, `devlog.rs`, the protocol value types, the wire validation, the `PowerMarks` seqlock, `RecordingLease` and `WorkerExit`.
