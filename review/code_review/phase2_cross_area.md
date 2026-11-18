# Phase 2 cross-area changes pending (apply after all agents finish)

## From E
1. core/src/platform/windows.rs `journal_operation`: add arm `JournalStep::Reserve => "Reserve lifecycle room in the journal queue",` (compile-required) -- DONE by B
2. core/src/platform/windows/input/receiver.rs ~798: `sampler.finish(emit)` -> `sampler.flush(emit)` (compile-required)
3. gate.rs injected-hook copy -> matching::injected_hook (C8, check if C did it)

## From F
4. F9 rest: core/Cargo.toml add windows-sys feature "Win32_System_Power"; environment.rs replace local extern RegisterSuspendResumeNotification/Unregister + PowerSubscription struct with windows_sys::Win32::System::Power items (HPOWERNOTIFY is isize; PowerRegistration.handle + null check; callback PDEVICE_NOTIFY_CALLBACK_ROUTINE with *const c_void). Optional, not compile-required.
5. F12: Remedy enum in error.rs + ProbeError::into_error carries it from mf.rs MF load failure + ui app.rs ~657 uses error.remedy() instead of message.contains. Needs A/B/F coordination.
6. F13: hotkeys.rs RecorderHotkeys::open takes notifier Arc<dyn Fn()+Send+Sync>, called on accepted activation when not active; ui app.rs register_hotkeys passes ctx.request_repaint, delete request_repaint_after(40ms) ~236.

(item 2 APPLIED by coordinator)

## From C
- B3: if B switches stills::dropped_trigger -> fallback_row(Trigger, Role,...), update input.rs route_fixture ("checkpoint","image")/("final","final_save_settled") and receiver.rs poll_pointer_rest ("pointer_rest","image").
- D1: if elevated_invalidation signature changes, input.rs poll_elevated Err arm must follow.
- C13 partial: pause/resume/stop blocks of pump still inline.
- Docs: REMEDIATION_PLAN gap rows (~882) + §6.1 rule 5: key records lost before classification are Sensitive count-only (resync_pending, foreground_inaccessible, producer_stall, unknown_device, uncertified_source, gate overflow, staging_pressure); pointer/HID keep ranges. JOURNAL_SCHEMA: input_health.suppressed no longer counts AltGr companion; §2.3 32-record pending health merge rule else transport_overflow. Device beyond manifest cap still reaches gate touchpad inventory; only device_inventory_truncated set.

## From D
- D1 input.rs else discard_unbound_notifications: APPLIED.
- D4 input.rs take_confirmed_text losses -> receiver.note_loss: APPLIED.
- D10 optional: remove `session` param from Selector::tick/stop (+ private helpers), use self.session_status; update input.rs ~630 source.stop, ~1141 source.tick.
- D8: if G exports collector CLEANUP, elevated/mod.rs should use it.
- Docs: ELEVATED addendum §6.1 lane receipt input-only, undecodable context payload stays protocol failure. Schema/ALG: elevated commit losses now input-stream TextEdit losses; elevated trailing window_location rows have trailing:true; stale elevated window notice emits timed_out. Remediation §6.2 row 18 / C3: ring loss before visit only counted; later one revokes visit instead of failing source.

## From A
- A7 typed ProbeKind: needs windows.rs (B) ProbeError kind field + with_kind/kind; then A swaps stage== comparisons. Follow-up.
- A8 NEW SERIALIZED FIELD CaptureCounters.frames_superseded (metadata.rs) + wgc newest count + monitor.rs add. Needs USER decision (schema change).
- A11 clock: protocol time.rs MessageTickAnchor::tick_delta_ms; coarse_estimate + clock.rs use it. Follow-up.
- A16 partial: monitor::capture format-reply block, Video 45 fields.
- Docs: §6.2 rows 2,9 (rotation part/segment close F only if stop deadline expired else L; encoder release failure at format change/Stop L); ALG §9.1/V4 software pacing newest frame per slot at slot end; metadata.rs video_software_paced doc "older frames of a slot replaced by a newer one"; V4 EncoderCandidateRejected also on selection failure; unreadable output affects only that monitor; §6.5 segment flush waits for last row released; NV12 pool up to 9; verifier COM failure = first part verdict.

## From G
- G18 session.rs RevocationUnknown reason + mapping + guidance: APPLIED.
- G1 deviation: TokenIdentity/ProcessIdentity in Worker::uia still -> OutsideApproval.
- G6 skipped credit/send_scoped rechecks. G9 side effect: image paths 4096..32767 now succeed.
- Docs: G3 stalled Recorder command/query write after readiness = transport (not readiness_deadline); G2 collector status write fails after 750 ms cleanup bound; revocation_unknown reason.

## From H
- H edited flush parts of elevated runtime/input.rs (Output fields/init, offer status lines, flush, flush_status, flush_loss; removed EnvironmentState import). Check merge with G.
- Losses::front/delivered now private -> callers use Losses::flush.
- Behaviour notes: status flush stops at first refused record; helper skips non-encodable status record.

## From B (stills.rs removed `stills::dropped_trigger`; compile-required)
B1. core/src/platform/windows/input.rs ~1537-1541 (fixture save fallback): replace the hand-typed `(trigger, role)` match and the `stills::dropped_trigger(...)` call with
    `let (trigger, role) = stills::save_slot(confirmation.kind());`
    `let mut fallback = stills::fallback_row(trigger, role, time, crate::protocol::Evidence::Observed(geometry), open.boundary.epoch, None, "save_handoff_unavailable");`
B2. core/src/platform/windows/input/receiver.rs ~1301 (pointer rest fallback): replace the `stills::dropped_trigger(...)` call with
    `let fallback = stills::fallback_row(stills::Trigger::PointerRest, stills::Role::Image, now, Evidence::Observed(motion.geometry), motion.admission, None, "pointer_rest_handoff_unavailable");`
B3. core/src/package/metadata.rs `PngCounters::unreferenced_images` doc: the count is now exact (images in `images` that no journaled still row names, plus revoked images whose removal failed), no longer a lower bound; drop the "Not counted: ..." sentence.

## Doc updates
- H: REMEDIATION_PLAN ~1626 (E4): env record after fresh unthrottled registration check, observed_qpc = QPC before check, kept across retries; stop/inventory records checked too.
- H: optional-collection notices (INP-14/15, M1-05) retained while queue full, delivered at stop.
- H10: scrubbing USERPROFILE/COMPUTERNAME/USERNAME only at token/path-segment boundaries, ASCII case-insensitive.
- H13 optional: hook rings single-owner per process.
- F16 optional: doc/SCREEN_FIX_M1_JOURNAL_SCHEMA.md:15 session-clock µs floored (also pre-origin).
- E: doc/SCREEN_FIX_M1_ALGORITHMS_ADDENDUM.md:434 Capacity bullet: lifecycle reserve (24) shared by gap batches/boundary/health; when full gap batch stays pending, boundary/health fails with distinct `reserve` journal failure (F).

## Follow-ups (after critic review), doc impact
- A3 REVERTED by user decision: software pacing = first frame of slot converted immediately (as before Phase 2). IGNORE the A-section doc items about "newest frame per slot" and the `video_software_paced` doc change; no doc change for pacing.
- A8 not done (user decision).
- E4/D3: a suspend without a resume yet is not counted as outlived by a collector seen live (collector seen live between suspend notice and sleep -> G after wake).
- §6.5: a segment with no rows requests its frames flush immediately; pending flushes are flushed when the merge finishes; otherwise a segment flush waits until its last row is released (A10).
- F13: UI idle 40 ms repaint poll only runs while previous attempt's hotkeys clear; hotkey pump notifies UI on accepted Start and on failure.
- F12: Media Feature Pack hint is a typed `Remedy` on `CaptureError` (UI text unchanged).
- G18: new source failure reason `revocation_unknown` with guidance text.
- B5: `unreferenced_images` is exact: every inventoried image no journaled row names in `file`/`seen_file`, plus revoked leftovers (ALG addendum ~§582 "unreferenced", REMEDIATION_PLAN ~1500).
- B3: fallback rows emit `frame_obs_id`, `seen_obs_id` (pre rows) and `geometry_evidence` as schema §6 already says; pointer-rest drop row has full row shape.
- B11/B15 (if documented): hotkey progress marked at least every 50 ms; still lane-stall exclusion uses awake time.
- B8: a candidate process that cannot be opened/verified fails validate_processes only if no approved process is found.
