# Developer handoff (stages completed by the previous developer)

## Conventions
- Files are LF (the git index uses LF). Check with `git ls-files --eol <path>`.
- Verification: `cargo check --workspace --all-targets` must pass with 0 warnings and 0 errors. Force a re-check of a changed crate if needed, e.g. touch a file or use `cargo check -p <pkg>`.
- Known per-package exceptions, not caused by the remediation:
  - `cargo check -p screen-fix-elevated --features elevated-release` needs release inputs in its build script. This is expected.
  - `cargo check -p screen-fix-native-observation --features context` fails in `events.rs` (`WNDCLASSW`/`RegisterClassW` not found). Confirmed pre-existing on the baseline; fixed in stage X1 (missing windows-sys features in the crate manifest).
- No git state changes. No test, fmt, clippy or app runs; only `cargo check` is authorized.
- After each stage, report and end the turn. The coordinator snapshots the tree and passes it to the critic. Critic remarks take priority over the next stage.

## Completed and approved
- **Step 0.** The baseline compiled with 0 warnings and 0 errors.
- **A1.** Loss ledger in `core/src/package/outcome.rs` and `journal.rs`.
  - Vocabulary: `LossReason`, `LossProducer`, `SuppressionClass`, `HealthKind`, `LossCount`, `LossScope`, `LossEvidence`, `GapRecord`, `DroppedTotal`.
  - `JournalIngress`:
    - `record_loss(LossEvidence, admitted)` for ordinary losses; sensitive losses are count-only.
    - Pending entries are emitted before data rows, but not while paused.
    - `submit_health(HealthRecord)` uses reserved capacity.
    - `pause()` / `take_paused_health()` / `resume()`; the resume row carries `suppressed_health`, and a Stop while paused writes `paused_summary`.
    - The field `last_admission`.
  - Frames go through `into_verified()` → `VerifiedIngress` (blocking only, no loss path).
  - `StreamSummary` counters; the manifest `health` block, reconciled by the reader.
  - Reasons defined so far: `queue_overflow`, `late_commit`, `staging_pressure`, `late_row`, `producer_stall`, `retention_unavailable`. Producer: `text_edit`. Suppression class: `pause`. Later stages add variants together with their producers.
  - Other gap-like kinds still use fixed strings and are left to their stages: `input_continuity_gap` (I1), `context_source_gap`, `recorder_control_gap`, `context_notifications_gap` (C3/E3).
  - Unavailable classification uses string keys `"focus:<status>"` for now; replace them with a typed enum in R1.
- **A2.** `core/src/session/fault.rs`.
  - `FaultReport` with class `Attempt` (F) or `Activity(Required|Auxiliary)` (L). `FaultLog` lives in the controller `Facts`. The first report is primary; F reports are capped at 32 and deduplicated.
  - `CaptureError::owned(component, …)`; `ProbeError::owned_by` / `into_error(owner)`.
  - `FailureComponent`: `Capture`, `Receiver`, `Context`, `Stills`, …
  - `MetadataOwner::finish_recording(AttemptFaults)` applies the rule-4 order. Manifest `health.activity_losses`. The UI shows "Recording degraded".
  - G-class losses never go to `FaultReport`; they belong in the ingress ledger.
  - PNG worker exit is still F. Stage P1 converts it to L-auxiliary per §6.2 row 16 (journal failure stays F, no double report; the UI shows degraded and the disposition stays recorded).
- **S0.** `PowerMarks` seqlock in `session/windows/environment.rs`; the only suspend store.
  - Accessor `WorkerSession::suspend_marks() -> SuspendMarks { sequence, suspend, resume }` and `suspended_for`.
  - Power and desktop callbacks use `catch_unwind` and no Mutex.
  - **Follow-up (critic).** I5, S1 and V5 must use one S0-side query, "suspend-excluded elapsed time over [a, b]", that returns an explicit unknown (not zero) when a mark lacks a QPC. Unknown never produces a fault. Do not subtract directly from the latest suspend mark.
  - S4 adds `catch_unwind` to `window_proc`.
- **V1.** `video::capacity_for(fps)` → `VideoCapacity { segment_pictures, staged_slots }`.
  - `SegmentEncoding.capacity`; `slots::Evidence::new(slots)` with a per-epoch check.
  - `JournalLimits::for_stream` / `with_rows_at_least`; frames journal rows ≥ monitors × (staged + 1).
  - The per-monitor lane capacity formula moved to R1.
  - **Follow-ups for V2a:**
    1. Make explicit that the sizing upper-bounds every effective fps: either a const assertion `SOFTWARE_FPS <= min pace`, or size from max(requested, `SOFTWARE_FPS`).
    2. Check whether staged rows carry over on a 60→15 fps reconfigure against the smaller bound, and handle it explicitly (note it for V3 if the logic moves there).
- **C1** (under critic review at handoff time).
  - `editable()` uses a tri-state per pattern; `NON_TEXT_CONTROLS` is kept for C2.
  - `focus::focus_window` falls back to `hwndActive`.
  - `UIA_E_TIMEOUT` and an exhausted budget → `TimedOut`; `matches()` returns `Result<bool, Status>`.
  - `has_ime` returns `Option<bool>`; `None` → `State::Unknown`.
  - `Client::budget` runs before every provider call, with `MIN_CALL` = 10 ms.

## Next
Follow the plan's §7 order: I1, I5, V2a, V3, V4, P1, C3, S1, S2, E2, E3, E4, X1, T3 → I3, I4, I6, I7, C4, P2, P3, P4, E1, T1 → S4, T2. The previously blocked stages follow their decisions (C2, C5, I2, V2b, V5, S3, E5, E6), then R1 → D1. I8 is dropped. T4 needs user authorization.

## C1 fix (hwndActive fallback)
- `FocusWindow { hwnd, active_fallback }`. `FocusSnapshot.active_fallback`: a fallback snapshot has `password_style: None` and is used only for layout and focus identity.
- UIA treats a fallback snapshot as not current, so the read is Stale and the target PossiblySensitive.
- The clipboard focus checks require `!active_fallback`.
- In the collector, `has_ime` maps `None` → `Unknown`.
- C1 and its fix are under critic review at handoff.

## Developer #2 (I1, I5, V2a)
- **I1** (approved).
  - `matching.rs`: connected components; outcomes Paired / Singleton / N raw-anchored ambiguous rows with `Cluster{raw, hooks, injected, excess_hooks}` (hooks absorbed).
  - `finish(limit)` with `InputStep::Undecidable`; `close_producers()`; typed `InputGap`.
  - `gate.rs`: `Outcomes` (losses and suppressions); `approved_receipt` returns `Result<_, SuppressionClass>`; `admit_outcomes`.
  - New `LossReason`, `LossProducer` and `SuppressionClass` variants in `outcome.rs`.
  - `STOP_FINALIZATION_RESERVE` = 2 s.
  - Ambiguous motion is counted as `motion_ambiguous` until I2.
- **I5** (fix under review).
  - Protocol `SCHEMA_VERSION` 8: `Producer::OptionalHid`, `OptionalCollection`, `HelperToCore::OptionalUnavailable`, `ProducerGap.cause`, `PacketError::InvalidCursor`.
  - The helper skips malformed packets and counts them.
  - `SuspendMarks::awake_elapsed(...)` → `AwakeElapsed::{Known, Unknown}`.
  - `input/blind.rs`: typed blind intervals with a `producers` field (`registration_lost`, `hook_lost`, `producer_stall`, `resync_unavailable`, `elevated_window`, `foreground_inaccessible`, `device_removed`).
  - Stall: blind at 250 ms awake, L at 5 s. Only packets of the stalled producers are dropped (I5 fix).
  - `input_health` per second (`raw_packets_coalesced`, `motion_rows_thinned`, `suppressed`).
  - The manifest has `optional_unavailable`.
  - Open for E3: the collector drops `OptionalUnavailable`; a core decode failure is fatal to the transport.
- **V2a** (under review).
  - `monitor.rs` `TimelineWait` holds one frame until the epoch timeline exists; otherwise the frame is counted as `timeline_pending`.
  - New `slots` tag `TIMELINE_PENDING` and `MissingVideoSlot::TimelinePending`.
  - `previous` is kept across same-size geometry changes (`Topology::capture_geometry`).
  - Counters: `VideoCounters.frames_{superseded, late, other_epoch, rejected}`; `CaptureCounters.video_discarded`, `video_pressure`.
  - `encoder::max_effective_fps(requested)` = max(requested, `SOFTWARE_FPS`) sizes the evidence ring and the frames journal.
  - Staged rows never carry over a reconfigure.

## Track A by developer #3 (I5 fix 2, V3, V4, V2b, V5, S1, S2, S3)
- **I5 fix 2.** HookHealth keeps a per-hook stall interval; transitions inside it are skipped.
- **V3.**
  - A per-monitor verifier thread runs `mp4::verify_segments`, with `VERIFIER_QUEUE` = 2.
  - Rename happens only after verification, with a claim flag.
  - Staged rows carry across format changes and pauses.
  - `segment_unverified` (row 19).
  - `capacity_for` doubles `staged_slots`.
- **V4.**
  - Display read failures produce `unavailable` maps; DPI and refresh are Option.
  - The `encoder::Probe` channel moves the self-check to the capture thread.
  - `rejected_encoders`.
  - `hdr_white_level_unavailable`.
  - `configured_peak_bitrate` / `configured_mean_bitrate`.
  - `WorkerSession::report_health` / `health_after`, bounded at 256, with overflow in `omitted_health` (manifest `HealthSummary.omitted_health`).
- **V2b.** `Selection.candidate` / `future` watermark; `collect()`; `SegmentEncoding.slot_decision_grace_us`; `monitor::CONVERSION_DEADLINE`; `arrived_after_decision`.
- **V5.**
  - `segment_deadline` is set only by Stop and session end. `suspend_wait` produces `suspend_finalize_late` health.
  - `pause_started` uses `awake_elapsed`.
  - The encoder watchdog, the elevated prefix wait and the conversion deadline restart on a suspend-sequence change.
  - `WorkerSession::cleanup_deadline()` (suspend bound `Instant`+2 s) is pending a Track B fix in E4.
- **S1.** Hotkey gap via `awake_elapsed` (`Facts.hotkey_suspends`); it only records `ControlLoss`; the latch is removed.
- **S2.**
  - `RetainedRecording.manifest_confirmed`; admitted failures are retained.
  - The countdown is held during lock, secure desktop or sleep.
  - Still losses are shown in the UI.
  - `SourceBlindReason::Pause(PauseReason)`.
- **S3.** `MonitorVideo.intervals_without_video`, `note_interval` and `usable()` drive the disposition and the inventory.
- **S4 (reduced).** Only `catch_unwind` in `window_proc` plus documenting the lifecycle states and latches. The full refactor is deferred.

## Track B by developer B (I3, I4, I2, I6, I7, C3, C2, C4, C5, E1)
- **I3.**
  - `Context::hook_receipt(qpc, clock)` → `Covered` / `FocusUncertain` / `Pending` / `Unknown`, based on visit history (`previous_visits (Visit, verified, ended)`, `swept`).
  - Gate `hook_only()` uses the timeline approval.
  - `injector_hints` reuse `ControllerHint`; `Context::prune_visits`.
- **I4.** `HeldSnapshot.keys [u64; 4]`; `PhysicalHeld.keys_up`; non-press downs get `repeat:null` + `held_across_boundary`; `press_unavailable` on an up without a press.
- **I2.**
  - Hook-anchored `decide_motion`; `wheel_run` / `decide_wheel`, with the 32 cap cut at an equal-count prefix.
  - Per-second `input_health.raw_motion`.
- **I6.**
  - `device::identify` runs on the `DeviceWorker` thread (queue 64).
  - `RawWindow` caches the process handle.
  - Hooks use a `OnceCell`.
  - `Ring::push` / `pop` use volatile raw-pointer access.
  - `InputHelper::drop` waits 750 ms.
  - `pen_pressure` `unavailable_unknown`.
- **I7.**
  - `LossReason` `CommitCapacity` / `TooLong` / `Parts`.
  - `uia_commits::Queue::take_loss() -> Vec<CommitLoss>`.
  - `ConfirmedText.valid` → `status:"invalid"`; `TextStatus::Unknown`.
- **C3.**
  - `screen_fix_native_observation::LossCounter { note(count, first, last), take() }`.
  - `Context::take_losses(receipt, at)`, `Context::take_health()` → `ContextHealth`, drained by `publish_context_health` in `input.rs`.
  - `ApprovedProcesses::poll() -> (unresolved_count, newly_lost)`, which never errors.
  - New `LossReason` `ClipboardQueue` / `NotificationOverflow`; new `HealthKind` `IdentityUnresolved` / `AuxiliaryActivityLoss`.
- **C2.**
  - `Writability` four-way; `TypingUnknown` in `TextClassification` / `TextTarget` (exhaustive matches need the arm).
  - Mixed is detected by identity with `ReservedMixedAttributeValue`; check `IUnknown::try_from(&VARIANT)` at compile.
- **C4.**
  - Ghost map; `window_location trailing:true`; `window_hint` rows; `pointer_query_busy`; `focus::password_style` checks the class.
- **C5.**
  - `Context::count_suppressed` / `take_suppressions`; gate writes `suppressed_interval` rows (reasons `&'static str`).
  - E5 `ineligible_token` can use the same `Suppression` list.
- **E1.**
  - `Selector::ordinary_allowed()` is false only when stopping or `Foreground::Elevated`.
  - `Receiver::drain_after_latch`.
  - `Selector::take_uncertified` + `Ingress::drain()` + `Receiver::note_uncertified` (`LossReason::UncertifiedSource`).
- **Elevated wire.** attachment `VERSION` = 2 covers `HeldSnapshot.keys`, `CommitEvidence.valid` and the I5 producers. Update the comment rather than bumping again.
- **E3 pending items.**
  - Forward commit losses per reason (the collector sums them in `runtime/context.rs` `commits()`) and count invalid commits.
  - The collector forwards `OptionalUnavailable`.
  - A core-undecodable packet is not fatal to the transport.
  - A typed input-stream marker for elevated loss.
- **E4 pending items.** Make the `WorkerSession::cleanup_deadline()` (selector.rs ~399, ~750) suspend-excluded; a sleep-caused miss is G.

## Pending D1 documentation fixes (from the critic's D1 review; do them with R1/D1-schema)
1. ALG §13.5: replace the non-existent reason names `raw_association_ambiguous` / `raw_unavailable` / `wheel_association_ambiguous` with the actual Evidence reasons in code (`ambiguous`, `not_observed`).
2. ALG §13.8: D-4b/D-4c sub-reasons are not journal fields. Remove the parenthesised names or say the distinction is expressed by the target class (`possibly_sensitive` with `editable:null` vs `typing_unknown`).
3. ALG §13.4: there is no Receiver-owned timeline. The Context visit history plays that role (`Context::hook_receipt`, `prune_visits`).
4. ALG §13.1: during data writes, gap rows use normal capacity and stay pending in the ledger. They are emitted with reserve capacity at boundaries, at flush and at close. Health rows use the reserve.
5. ELEVATED §6.4 / §6.5 (D-8 A, D-9 A): verify against the actual E5/E6 code and describe what was implemented.
6. ELEVATED §6: add
   - the E3 token-check trade-off (token facts are checked on prepare, renewal, focus refresh, activation and once per egress batch; the window/thread/desktop binding is checked per record; the residual window is within a batch);
   - the row 6 timing (decision at request + 750 ms, termination after the cleanup bound);
   - lossy receipt of an undecodable payload, while scope or kind mismatch stays fatal.
7. ALG §13.2 row 7: control/status writes are effectively bounded at 750 ms by `owner_healthy`.

## Track B by developer B2 (E1 fix, E2, E3, E4, E5, E6, row 17)
- **E2.** `Worker::call(kind, request_deadline)` with the slot at request deadline + 750 ms; atomic `Pending` state RUNNING / HANDED_OFF / TIMED_OUT; `Owner::expire_reads`; `publish_item` turns a post-deadline result into `Rejected(TimedOut)`.
- **E3.**
  - Wire: `ContextGap.cause` (`ContextGapCause`) and `InputEvidence::OptionalUnavailable` (VERSION 2).
  - Per-reason commit losses.
  - `receive_lossy`: only `InvalidPayload` is lossy; scope or kind mismatch returns `InvalidRecord` (fatal).
  - `Receiver::note_elevated_loss`; `Ingress::malformed`; prior gaps via `Selector::take_prior_gaps()`.
  - Token facts are latched via `Grant::verify_token` per batch.
  - Lanes use `ESSENTIAL_PROGRESS`; the Recorder's `WRITE_BUDGET` is 1 s.
  - Still open for R1: the fixed-string `recorder_staging` / `optional_staging` `context_source_gap` rows in `input.rs`.
- **E4.**
  - `ElevatedSaves::poll(.., closing)` gives final checkpoint rows (`unavailable`: `admission_closed` / `timed_out` / `queue_full`).
  - `Selector::cancel` keeps the host, and Stop waits for `Stopped`.
  - `SourceRegistration.held`; `ElevatedSourceEvidence` `consent_dispatched` / `consent_returned` / `withdrawn_launches`.
  - `WorkerSession::request_input_check()` + `Receiver::helper_checked()`.
  - Bootstrap DACL uses the client mask.
  - `Secret` is filled in place inside a `Box`.
  - Sleep-caused collector loss is G (`HealthKind::ElevatedSourceSleep`), after which the selector resets.
- **Row 17.** Clipboard worker exit reports `AuxiliaryActivityLoss` once.
- **E5.** Target or focus changes during preparation revoke instead of failing; the startup candidate is prepared again until its readiness deadline; ineligible-token foregrounds open a C5 `ineligible_token` suppression.
- **E6.** `native-observation identity::canonical_image(path)`; `Grant::prepare` compares the canonical image of the pinned process with the sealed approval key.
8. ELEVATED §6.1: the "bump `attachment::VERSION` on any `ipc/elevated` type change" rule applies from the first release. Within this development cycle, several wire changes share VERSION 2.
9. ELEVATED §6.5: the code does NOT compare a file ID (`GetFileInformationByHandle` is used only to reject directories). State that no file ID is compared because the approval key carries no file identity. The capture target is the pinned process, so replacing the file at the same path cannot redirect capture.
10. E-stage behaviour to document: a collector lost across sleep is G (`elevated_source_sleep` health). There is no relaunch in the same attempt; later elevated foregrounds are suppressed (C5).
11. ELEVATED/D1: the prepare-time `arm` QPC is never later than the reported Arm QPC; every use treats it as a conservative lower bound.
