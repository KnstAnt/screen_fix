# M1 specification addendum and implementation scope

Date: 2026-11-15 (Europe/Moscow)
Project: screen_fix / Intercall Recorder
Baseline: Intercall Recorder technical spec v1 Windows.pdf, especially section 17.

## 1. Authority and status

This document records the accepted M1 scope amendments and the practical implementation contracts that accompany them. It overrides the PDF only where an amendment is explicitly stated below. All other applicable PDF requirements remain in force. The PDF and target.txt are unchanged.

PDF text defines behavior; interface screenshots illustrate appearance and do not override behavior. M0 is treated as completed and is not reopened as a prerequisite to M1. This is a requirements and scope document, not a statement that M1 code or tests are complete. The [project specification](SCREEN_FIX_SPEC.md) and [capture architecture](SCREEN_FIX_ARCHITECTURE.md) summarize the current M1 scope. Read this addendum with the PDF and follow the [elevated capture specification addendum](SCREEN_FIX_ELEVATED_COMPONENT_SPEC_ADDENDUM.md) for the approved exception.

Project code, user-facing strings, comments and documentation remain in English. Existing project and critic prompts may remain in Russian.

## 2. Accepted amendments

### A1. Separate implementation completion from hardware acceptance

Track two independent outcomes:
- M1 implementation completion: the scoped functionality exists, is integrated, and available verification is reported accurately.
- M1 hardware acceptance: the required measurements and hardware scenarios have been executed and their results recorded.

A special measurement rig and the two-week input bench are deferred and do not block coding completion. Available equipment consists of a Windows 10 machine, a Windows 11 machine, and a touchscreen with a pen. Exact OS builds, GPUs, resolutions and pen capabilities must be recorded when testing; they are not assumed here.

T8b and the full T20 matrix remain pending until suitable equipment is available. Unsupported or unmeasured device profiles must not receive invented calibration values or a passed status. The available machines do not establish coverage of every GPU vendor, peripheral or security product.

Plan and perform the applicable partial checks on the available Windows 10, Windows 11 and pen equipment separately, subject to execution authorization. Do not postpone those checks merely because the special rig is unavailable. Record their results independently of implementation completion; the full bench remains pending.

### A2. Defer automatic fault recovery

For M1, detected failures produce a clear user-facing error and a local diagnostic report suitable for manual submission. Report generation does not imply automatic transmission.

Diagnostic reports are written directly into the selected recording output directory, including preflight failures before an attempt exists. A temporary directory is used only for errors before any output directory has been selected. Failure to write to a selected directory is reported as unavailable; it must not silently redirect the report elsewhere. Report workers handle recording diagnostic file I/O.

Deferred to the next development stage:
- REL-3 startup recovery scan, repair/truncation of damaged journals, segment repair and automatic recovered-prefix reconstruction.
- REL-5 replacement of stalled workers, abandoned-worker generations and automatic process restart.
- Automatic input-helper restart after failure.
- Runtime recreation/retry of a failed encoder or GPU capture device and automatic fallback used to recover from that failure.
- Recovery acceptance criteria attached to those mechanisms.

M1 must not silently continue presenting a failed recording as healthy. Stop the affected activity and report the recording as failed/incomplete when required data can no longer be produced. Do not delete already saved data. Perform ordinary bounded cleanup where possible; do not introduce recovery machinery under the name of cleanup.

Normal startup capability selection is retained: when no usable hardware encoder is available, the PDF software-encoding path may be selected before recording. This is distinct from runtime fault recovery.

Normal operation remains required: segment rotation, ordinary Stop, manual pause/resume, display configuration changes, lock/sleep boundaries, queue accounting and disk-space controls. A normal display topology change is not a reason to defer all VID-9 behavior. Actual device failure is diagnosed rather than automatically recovered.

Diagnostics are best effort when the failing resource prevents report creation. Show the error in the UI when possible and do not claim that a report was saved if writing failed. No in-process diagnostic guarantee is made for power loss or complete process termination.

### A3. Memory is bounded, but 300 MiB GPU usage is a target

The PDF 300 MiB GPU-memory figure per 4K monitor is a measurement target, not a hard M1 acceptance limit. Do not degrade required image quality or introduce complex memory management merely to meet it.

Use simple bounded buffers sized for selected resolutions and monitor count. Account for simultaneous retained frames, not just ring slots. Do not let pending jobs create an unbounded collection of pinned textures.

The overall resource objectives remain relevant and must be measured with the recorded application running. Neither several gigabytes of dedicated VRAM nor a particular memory architecture is a platform guarantee. Buffer calculations are not measurements of total application or driver usage.

### A4. Keep the implementation simple

Use concrete owners, bounded queues, typed values and explicit states. Retain useful existing capture and encoding code. Avoid a generic scheduler, universal worker recovery, generic GPU allocator, or global stream-reordering framework.

Do not implement M2-M4 product features merely because their interfaces are referenced by the capture core. Preserve necessary extension points through small value-based contracts.

### A5. Approved elevated targets (ELC-1–6)

Amend PDF INP-13, the corresponding Start preflight prohibition and the treatment of blind boundaries in gate 3 for a positively identified approved elevated process/window on the Recorder's unlocked interactive desktop. The separate demand-started collector supports the same interactive user, Windows session and logon context, including the linked UAC split-token case. Secure desktop, lock, suspend, the UAC transition, uncertain identity, cross-user elevation, other sessions/desktops, SYSTEM and protected targets remain outside admitted elevated coverage.

The Recorder and its standard input helper use the existing launch token. A normal launch from the built-in Windows Administrator can already have high integrity. The Recorder does not request elevation for itself. An approved foreground within the inherited token’s access scope uses the standard source, with its existing readiness, privacy and publication rules. The [elevated component addendum](SCREEN_FIX_ELEVATED_COMPONENT_SPEC_ADDENDUM.md) defines the token checks. Protected preparation is reserved for a positively identified foreground requiring the separate collector and a supported medium-to-high split pair. It requires the matching signed x64 payload in the machine-wide protected installation, verified at launch and attachment. The collector has a finite target-observation contract and lifetime. The Recorder retains the UI, controller, WGC, encoding, stills and all output writers.

Before starting or resuming a collector-backed visit, keep target publication closed while both sides establish exact independent live identity and desktop proof, authenticate the peer, verify required producer readiness and resolve the armed held-state probe. Open the controller epoch on the normal QPC boundary, then require exact activation and fresh same-source resynchronization before collector evidence and the associated media become eligible. A valid typed unknown held result carries no invented edges; unknown required access stays closed. Scoped hooks and text-edit payload copies require fresh witness progress and current attachment and grant QPC leases. Revocation, loss of proof, lease loss, Stop or parent death ends target work under bounded independent cleanup.

Decline, timeout, unsupported deployment or required source failure before admission ends startup without an admitted recording. Stop before admission cancels it. An admitted source failure closes the earliest safely known per-visit prefix and finalizes a failed attempt under the existing bounded failure flow. Transition and uncertified time remain explicit. Relaunch, automatic recovery and retrospective removal of already published evidence are outside this amendment.

### A6. Remediation amendments (user decisions, 2026-11-17)

These amendments follow the M1 remediation plan (`SCREEN_FIX_M1_REMEDIATION_PLAN.md`, section 5). Each one overrides the cited PDF requirement for M1 only.

**A6.1 Registration and hook loss is detected, not repaired (D-2 B; amends INP-1 and INP-12).**
- M1 does not re-register Raw Input collections and does not reinstall low-level hooks.
- A lost required registration, or a required hook confirmed lost by hook health, opens an input blind interval and writes an `input_health` record.
- The loss is a required activity loss of input (L-required): the interval stays open until Stop, and the disposition is `input_unavailable`.
- A lost optional collection (touchpad or multi-axis HID) is recorded as unavailable and does not affect the disposition.

**A6.2 An unapproved foreground is suppressed, not paused (D-3 A; amends CTX-6 and PRIV-2 for M1).**
- M1 does not auto-pause on an unapproved or `always_pause_window` foreground and writes no `late_pause` record. The unified pause-cutoff policy stays in M3.
- Each suppressed interval stays observable:
  - a `foreground` row with `approved:false`;
  - suppression intervals with their reason and bounds;
  - suppressed-input counts per class in the stream summary, without per-key timing.
- Gate 4 has no pause interval for these periods. Gate 3 excludes suppression intervals instead.

**A6.3 Suspend finalization is bounded and continues after wake (D-5 A; amends CLK-7).**
- On `PBT_APMSUSPEND` the controller waits at most 2 s for each monitor to close its open segment, then lets the system sleep.
- A monitor that misses the bound writes a `suspend_finalize_late` health record. It finishes finalization and verification after wake and publishes the segment normally.
- The attempt continues in Paused(Sleep). Sleep never fails the attempt.
- Session end (REL-9) keeps its own budgets and the leave-`.part` rule.

**A6.4 Complete uses the interval rule for selected monitors (D-6 A; clarifies "usable video for each selected monitor").**
- Complete requires usable video for each selected monitor in each interval in which that monitor was present and admitted.
- Slots of an absent monitor are recorded as `monitor_missing` and do not prevent Complete.
- An admitted interval of a present monitor counts as without video only if it has no encoded slot and contains a slot that starts later than the interval start plus the slot decision grace G. Shorter intervals, for example Resume followed at once by Stop, are counted separately in `short_intervals_without_video` and do not prevent Complete.
- Approval-suppressed slots lie outside admitted intervals.

**A6.5 GPU device loss fails the attempt in M1 (limitation under A2).**
- A capture-thread failure that is not a topology change, for example GPU device loss, is an attempt failure.
- The VID-9 device-loss rebuild is deferred with the other recovery work. The PDF statement that such an event does not end the attempt is therefore not met for device loss in M1.

**A6.6 Checksums and gate 8 start in M2 (clarification of DATA-3).**
- PDF section 17 lists the M1 scope without `checksums.sha256`, and M1 acceptance uses gates 1–4 only. Gate 8 first appears in the M2 acceptance (T18).
- M1 packages carry no `checksums.sha256`. This is not an M1 defect.

## 3. Implementation contracts

### C1. Time and observations

Use the PDF single QPC timebase and retain receipt time, source time when available, and frame capture time separately. Missing source time remains unknown.

Apply the PDF before/seen selection rules with an identified calibration when available. A p99.9 correction is statistical, not an absolute guarantee about physical actuation. Missing calibration or a missing eligible frame is explicitly uncertain. Never substitute a later image and label it a verified pre-action image.

### C2. Capture buffers and pacing

Start with three WGC buffers, a four-frame native BGRA history, one retained settled candidate, one staging readback resource and one PNG worker. The ten image-sized units in the baseline calculation are a planning subtotal, not a peak capacity limit or proof that concurrent snapshot requests fit without additional retention.

Pending snapshot requests use a bounded retention capacity with the PDF priorities and explicit dropped-job records. A frame already retained for several reasons is shared as immutable image data rather than copied for each reason. Resources return to the owner only when their actual consumers are finished.

Keep capture/still selection pacing separate from video sampling. Software video at 15 FPS must not silently reduce still-selection timing precision. No second capture pipeline is required.

Retain the candidate image while confirming the 500 ms settled interval; reset it when the stability condition fails. Settled means visual stability, not completion of a native application operation.

Full M3 delayed-publication/privacy processing is deferred. M1 must leave a clear publication boundary but need not preallocate an unused 200 ms full-resolution queue. A stub allow-list is not a claim of production privacy protection.

### C3. Raw Input and hook matching

Use small bounded pending queues and match by event kind, available identifying fields and time. Consume each source event at most once. Ambiguous or expired matches remain unknown with a reason. Clear pending matches across discontinuities.

Keep late GetCursorPos observations separate from event coordinates. Do not guess an event position from a later cursor reading. Preserve evidence of origin and matching rather than claiming physical-device provenance without evidence.

One Receiver selects one native source at a time. Match within the same source visit, keep original QPC and producer progress, and require collector input to fall inside both its certified input and context coverage. Keep keyboard/mouse hook health independent and preserve pen/HID provenance and bounded loss/resync.

### C4. UIA and text

Use a dedicated UIA executor with bounded requests and the PDF timeouts. Associate requests with a focus generation; stale results must not affect a newer focus.

Buffer affected keys only for bounded classification time. Apply the PDF conservative redaction when editability/password status cannot be safely established. Distinguish text inferred from key/layout state from text confirmed by an actual commit source.

Each journal has one writer. Preserve its ordering contract without globally merging all streams. Delayed results retain their request/source timing and are not backdated into an already published sequence.

For collector-backed visits, bind native context, UIA, IME and commit results to the authorized process/window, visit, revisions and request deadline. Native password evidence stays authoritative. Check a distinct point target independently. Checkpoint context and fixture Save routing require the current selected foreground and visit at the original control or Save receipt. Protected context or routing is eligible only when that receipt precedes the exact visit's exclusive certificate and the bounded staging and proof checks still pass. The Recorder control row keeps its action, outcome and request ID when context is unavailable. The Recorder owns optional clipboard content. Pre/post fences and the same current focus and clipboard sequence control eligibility.

### C5. Pause and input state

Use one explicit recording state machine. Close known held keys/buttons with marked synthetic boundary events, then enter pause. Do not manufacture real transitions for activity inside a gap.

Resume with the available state synchronization, explicitly marked synthetic. The common clock continues across pauses. Start a new video segment after pause with a documented segment-local PTS mapping to the common clock.

Source switches use the same close and reopen boundary, preserving the attempt and clock. Invalidate the old matching, focus, held and provisional-still scope. Resume with current-source evidence after all independent pause reasons clear. A clean attached elevated collector can stay idle with no target reads. A detected move from the standard source to the collector latches the old epoch closed to new source work, including fixture saves and checkpoint context, even if foreground returns before Pause. A new live preparation with a configured idle collector requires fresh approval of the actual foreground.

### C6. Finalization and quality

Separate file/package finalization status from data-quality gaps. A correctly finalized package may still contain explicitly recorded losses; it must not be described as gap-free solely because Stop succeeded.

Retain bounded queues, gap accounting and normal flush/finalization requirements. A flush interval alone does not bound the age of unwritten queued events. Do not claim an absolute loss window that excludes queue backlog or failed writes.

Finalize the minimal manifest last. The metadata owner can begin atomic Complete publication only after required finalization and flushing have succeeded, the admitted prefix is certified, and the owner obtains authorization before the Stop deadline. Failure or deadline expiry before authorization leaves an incomplete result and diagnostics.

If Complete publication was authorized before the deadline, the UI stays responsive and reports final publication pending after that deadline. The same owner retains exclusive ownership until Windows returns. Windows success or failure determines the final result, with the native error included in diagnostics on failure. The shorter shutdown and suspend budgets still apply to their respective waits. Automatic repair is deferred.

An admitted protected-source failure retains the first substantive cause and bounded cleanup evidence. Unpublished still and media work must fit the exact visit and exclusive cutoff. Keep previously verified artifacts in the retained failed package when publication succeeds. The UI identifies that package only after confirmation. Pause and Stop settle only the certified retired prefix under their existing bounds. The terminal collector acknowledgement establishes host cleanup. The independently received input and context progress establishes how far the prefix can extend. Retain any staging loss with its original first-uncovered QPC and producer gap ranges, including on Stop while paused.

## 4. Frame-memory planning reference

All values below are MiB (1,048,576 bytes). BGRA8 uses width * height * 4 bytes. Texture alignment, driver overhead, encoder surfaces, PNG workspace, queues and UI allocations are additional.

| Allocation | 1920x1080 | 2560x1440 | 3840x2160 |
| --- | ---: | ---: | ---: |
| One BGRA8 frame | 7.91 | 14.06 | 31.64 |
| Four-frame history | 31.64 | 56.25 | 126.56 |
| Three WGC frames | 23.73 | 42.19 | 94.92 |
| WGC + history + one settled candidate | 63.28 | 112.50 | 253.13 |
| One staging resource + one CPU image | 15.82 | 28.13 | 63.28 |
| Baseline image-buffer subtotal | 79.10 | 140.63 | 316.41 |

This subtotal is not a peak memory limit. Each additional retained 4K BGRA frame adds 31.64 MiB. Pending-job retention must be included in the actual allocation limit. Staging placement must not be assumed to be entirely dedicated VRAM.

Three FP16 WGC frames instead of BGRA8 add 94.92 MiB at 4K, before extra tone-mapping workspace. One 4K NV12 encoder surface adds approximately 11.87 MiB; the actual number of surfaces is implementation-dependent.

An independent 200 ms BGRA queue would require at least six frames at 30 FPS or twelve at 60 FPS: 189.84 or 379.69 MiB at 4K, without scheduling margin. Overlapping retention reasons can share frames; do not count them as mandatory independent copies. This future privacy cost is not silently included in the M1 subtotal.

Two SDR 4K monitors have a baseline image-buffer subtotal of approximately 632.81 MiB, before encoders, pending jobs and other allocations. Measure actual total and GPU usage rather than asserting compliance from these calculations.

## 5. Required M1 implementation

The following is an implementation checklist, not a list of completed work.

### M1-01. Platform and lifecycle

- Preserve the Rust application, egui/eframe UI and separation of common logic from Windows modules.
- Follow the new PDF Windows baseline; do not infer platform support from merely owning a Windows 10 machine.
- Provide the minimal controls needed to select monitors/output directory, start, pause, resume and stop.
- Show recording state, failures and the diagnostic-report path.
- Use the existing native directory-selection flow.
- Keep callbacks short and nonblocking, and long-running capture, input, UIA, encoding and file work off the UI thread.
- Show consent, readiness, certified enhanced capture, pre-admission unavailability and admitted failure as distinct outcomes. Close requests during capture submit Stop; a visible incomplete result can be acknowledged and exited.

### M1-02. Common clock and event schemas (CLK)

- QPC origin/frequency and integer timestamp conversion; UTC anchors.
- Separate source and receipt timestamps, stream sequence numbers and uncertainty.
- WGC timestamp conversion and the prescribed handling of coarse Windows timestamps.
- Pause, lock/sleep and display discontinuity records.
- Frame IDs and explicit mapping between capture time, video slot and segment PTS.
- Instrumentation for later timing measurements without fabricated calibration.

### M1-03. Monitor capture and image processing (VID)

- Selected monitor set, stable identifiers, physical geometry and DPI awareness.
- Native-size WGC capture, no aspect-ratio stretching; only the PDF encoder-limit/padding exceptions.
- Cursor-free capture, immediate release of borrowed WGC frames after copying.
- Capture-owner control of GPU context operations and readback.
- GPU thumbnail change scoring rather than full-resolution CPU readback for every frame.
- SDR capture and the specified HDR-to-SDR processing/metadata.
- Ordinary resize, scaling and monitor removal/return handling with geometry versions and segment boundaries.
- Actual device failures follow A2.

### M1-04. Video encoding and segments (VID, REL)

- Hardware H.264 path and startup selection of the software 15 FPS path when hardware encoding is unavailable.
- PDF encoding profile, bitrate policy, no B frames and keyframe policy.
- Video sampling independent of still capture, with repeated-frame provenance and encoded/missing-slot accounting.
- Approximately 60-second independently decodable segments with required IDR/header boundaries.
- Bounded handoff during rotation, ordinary finalization and temporary-to-final file publication.
- Manual pause/resume segmentation and source-time/PTS mapping.
- No automatic runtime encoder recovery in M1.

### M1-05. Input helper and raw input (ARCH, INP)

- Separate helper process with the specified restricted IPC/shared ring and parent-lifetime coupling.
- Raw keyboard/mouse input and low-level hooks on their required message-loop threads.
- Device discovery, origin labels and bounded Raw Input/hook matching.
- Normalized key identities, modifiers, repeats, AltGr/dead-key handling and layout changes.
- Mouse buttons, wheels, event coordinates, monitor identity, held state and coalescing/warp evidence.
- Preserve held-motion/stroke samples; apply the PDF background sampling rules.
- Control hotkeys and exclusion of recorder-control input as specified.
- Gap/blind accounting and synthetic state boundaries; helper failure is diagnosed without restart.
- Keep SHOULD/MAY device requirements at their PDF strength; do not silently promote optional MIDI or best-effort device support to mandatory M1 hardware coverage.
- Keep the standard restricted helper and the separately authenticated elevated collector on the shared native producer and core Receiver path. Require actual selected-source registration and visit readiness.

### M1-06. Pen and extended-device evidence (INP-11)

- Implement the specified HID/Windows Ink pen path, device descriptors and report decoding.
- Record available pressure, tilt, buttons and stroke/contact state with timing and origin linkage.
- Explicitly mark unsupported or unavailable measurements; do not synthesize pressure.
- Preserve the PDF limits on Wintab and other device modes.
- Prepare to verify on the available touchscreen/pen; broader vendor/mode coverage remains pending.
- Preserve selected-source device namespaces and supported native HID evidence across switches; only validated device descriptors participate in existing calibration identity.

### M1-07. Context, focus and text (CTX, INP)

- Foreground transitions, approved-window identity/geometry, dialogs and layout context.
- Cursor shape/visibility track and display topology records.
- Focused-element and pointer-target UIA records with request timing, geometry/focus identity and unknown outcomes.
- Dedicated bounded UIA processing, stale-result rejection and hung-app precautions.
- Text/shortcut distinction and text commit handling under the PDF evidence rules.
- Conservative password/unknown-field redaction, including the per-visit aggregated record contract.
- Stub allow-list for M1 scenarios; no full accessibility-tree capture.
- Optional clipboard capture remains opt-in/off by default with the PDF ownership checks and bounded isolation.
- Route exact collector-backed target reads and editable-focus fences through the current grant; keep layout, privacy, cursor, geometry and clipboard decisions in the Recorder.

### M1-08. Native stills (IMG)

- Lossless live PNG extraction from native GPU frames, not decoding screenshots from H.264 after Stop.
- Implement applicable start, pre-motion, pointer-rest, pre-action, next, settled, observe and final triggers.
- Provide a narrow confirmed-save/checkpoint input contract; use a test producer until M2 supplies real application adapters.
- Before/seen selection with calibration evidence or uncertainty.
- Exact pixel-content deduplication including image dimensions/format/color contract, excluding timestamp and cursor metadata.
- Preserve trigger/event references even when several observations reuse one stored image.
- Bound retained images/readbacks/PNG jobs; apply PDF priorities, rate policy and explicit dropped-job records.
- One PNG worker initially; increase concurrency only when measurement justifies it.
- Link Action, Rest and Save requests from a collector-backed visit to the original certified evidence, visit and geometry. Expired or excluded evidence has an explicit unavailable disposition; durable input references are allocated by the owning journal.

### M1-09. Journals, minimal package and diagnostics (DATA, REL)

- Append-only raw input/context/frame/observation and applicable health/clock records using the PDF schemas.
- One writer per journal, bounded queues and explicit overflow/loss accounting.
- Normal periodic flush, pause/Stop flush and disk-space controls.
- Minimal manifest with selected displays, clock/segment mapping, actual recording modes, output inventory and truthful completion/quality state.
- Bounded ordinary Stop, required segment finalization and manifest publication.
- Clear English error messages and local diagnostic reports with technical cause and affected component.
- No automatic upload of diagnostics; no recovery scan or automatic damaged-file repair.
- Include optional selected-source intervals, protected setup and stop observations, and blind/uncertified reasons in diagnostics and v5 metadata. Retain the standard helper fields alongside the optional selected-source evidence.

### M1-10. Verification support and evidence

- Provide reproducible checks for journal ordering/parsing, frame/segment mapping, input boundary consistency and pause-stream rules (T1 gates 1-4 scope).
- Provide timing and queue instrumentation required for later measurements.
- Keep static review, compilation and runtime results separate.
- Report performed checks, failures and pending scenarios explicitly.
- Do not equate implemented instrumentation with completed hardware tests.
- Existing restrictions on running builds, formatters and tests still apply until separately authorized.
- For the elevated amendment, examine positive identity, trusted deployment, per-source registration, transitions, scoped reads and retained-prefix closure through real entry points. Record runtime, release-signing and hardware evidence separately.

## 6. Revised M1 acceptance mapping

| PDF test | M1 handling |
| --- | --- |
| T1 | Retain long-session capture and gates 1–4 scope. Gate 3 includes the ELC-1–6 same-session approved elevated exception, actual readiness and blind/uncertified boundaries. Include standard capture with the usual token and with inherited administrator rights, and both handoff directions between the standard source and the collector. Execute when the required configuration is available; record missing coverage. |
| T2 | Recovery assertions deferred under A2. Ordinary segment/flush behavior, helper parent-lifetime cleanup and independent bounded collector termination remain implementation requirements; do not claim full T2 passed. |
| T3 | Retain normal scaling and display topology transitions. Device-loss fault recovery is deferred. |
| T7 | Retain held pointer sampling and modifiers; derived-action interpretation remains excluded. |
| T8 | Retain software timing instrumentation and test procedure. Execution/measurements are reported separately. |
| T8b | Dedicated hardware timing rig deferred. |
| T11 | Retain layouts, IME, password handling and injected Unicode with the stub allow-list. Untested configurations remain pending. |
| T20 | Full two-week peripheral/security/hardware bench deferred; use available Win10/Win11 and pen equipment for partial evidence. |
| T22 | Retain still triggers, timing provenance, deduplication, priorities and write-delay measurements. M2-dependent signals use test producers, not a premature production adapter. |
| T23 | Retain bounded queue/gap behavior, low-disk control and HDR behavior. Replace helper/worker/encoder recovery expectations with failure notification and diagnostics for M1; original recovery assertions remain deferred. Include pre-admission cancellation/failure and admitted protected-source failure with the retained prefix. |

M1 implementation completion requires integration of the mandatory scoped paths, with concrete verification status for each. Hardware acceptance requires evidence from the applicable hardware scenarios. Neither a source review nor a successful build proves timing, memory, performance or hardware compatibility.

## 7. Explicitly outside M1

- Automatic fault recovery and repair, now deferred to the next stage.
- Completion of the hardware timing rig and full two-week input bench.
- M2 task/attempt workflows, native file checkpoints/reopen verification, pack registry and production Blender/FreeCAD adapters.
- M3 full privacy enforcement, blackout, own-window exclusion and review cuts/blur. M1 stub testing is not a privacy release.
- M4 derived actions/native effects, full release/eligibility gates, eight export formats, MCAP/training views and upload.
- Additional-platform implementations; keep the code separation without claiming macOS/Linux capture support.

No scope beyond these explicit amendments is removed from the PDF.

