# M1 development plan

Date: 2026-11-15 (Europe/Moscow)
Project: screen_fix / Intercall Recorder
Status: development plan; stages are not marked implemented.

## 1. Scope and authority

Implement the capture-core M1 defined by:
- SCREEN_FIX_M1_SPEC_ADDENDUM.md.
- Intercall Recorder technical spec v1 Windows.pdf, subject to the accepted scope amendments.
- SCREEN_FIX_M1_ARCHITECTURE_ADDENDUM.md.
- SCREEN_FIX_M1_ALGORITHMS_ADDENDUM.md.

Use the architecture's dedicated controller thread; the earlier optional UI-thread placement in the algorithms document is not selected. M0 is already completed.

This plan includes development only. It contains no compilation, cargo check, formatter execution, application launches, test execution, test-suite authoring, benchmark execution or hardware acceptance stages. Existing test files are not deleted to hide API changes; any necessary test adaptation belongs to a separately authorized validation task.

Implement product-side runtime checks required by the specification (for example encoder capability checks, record consistency and errors) as application code. This is different from running those checks or adding development test tasks.

Independent critic review is source/document review only. It may identify code defects and missing integration; it is not evidence of compilation or execution.

Automatic fault recovery is outside M1. Full production privacy/review, application packs/adapters, task/file workflows, derived exports and upload remain in their later milestones. Hardware testing is separate.

## 2. Execution rules

1. Work from the actual selected source baseline. The older architecture's implementation descriptions are migration hints, not proof of current code.
2. Preserve working native algorithms and existing UI where compatible. Replace obsolete paths through their real callers; do not leave parallel recording implementations selected accidentally.
3. Finish each stage's explicitly scoped production connections before marking it developed. Where a later stage owns the consumer, record that dependency and do not claim the whole feature is complete or insert fake success. P10 and P13 must close those declared integration dependencies; a module or trait alone is never completion of an end-to-end feature.
4. Keep errors and unknown/unavailable evidence explicit. No fake success, fabricated calibration, placeholder files or silent data-loss paths.
5. Keep common code independent of Windows types. Windows APIs and any justified unsafe code stay in narrow platform modules.
6. Do not implement future services merely because an extension boundary exists. No empty threads, generic plugin runtime, universal context object or scheduler.
7. Record source-level completion and remaining limitations, not passed runtime acceptance.
8. No full archive is required by this plan; produce one only on request.
9. Save source/document changes in the project. Present small code changes as before/after, not unified diffs; provide complete changed files when changes are substantial. Do not replace saved source changes with a prose-only description.

For each stage, the author supplies the changed-file list, fulfilled items and unresolved issues to the critic. Correct confirmed defects within the stage and review the corrected source. Do not invent a refactoring task from a preference alone.

## 3. Stage overview

| Stage | Development result | Requires |
| --- | --- | --- |
| P00 | Source baseline and responsibility mapping | Existing project |
| P01 | Workspace/module boundaries and typed contracts | P00 |
| P02 | Common clock, controller and lifecycle | P01 |
| P03 | Package IO, journals and diagnostics foundation | P01-P02 |
| P04 | Native per-monitor capture and resource ownership | P02-P03 |
| P05 | Video encoding, slots and segment rotation | P04 |
| P06 | Separate input helper and bounded IPC | P01-P03 |
| P07 | Input matching, normalization and held state | P06 |
| P08 | Context, UIA and text publication gate | P02-P03, P07 |
| P09 | Pen and extended input support | P07-P08 |
| P10 | Native still selection, readback and deduplication | P04-P05, P07-P09 |
| P11 | Integrated pause, topology, disk and Stop behavior | P03-P10 |
| P12 | Complete minimal UI and startup capability flow | P02-P11 |
| P13 | Complete M1 package, source integration and documentation | P00-P12 |

This is the default sequential order. The dependency column identifies prerequisites, not authorization to split ownership or start parallel agents automatically. Existing controls remain usable in source throughout migration; P12 completes their new bindings rather than introducing the UI from scratch.

## P00. Establish the source baseline

Work:
- Resolve the current authorized source tree/archive and its project instructions.
- Read the existing entry points and affected source paths in sequence.
- Map the actual UI, controller, WGC/GPU, encoder/muxer, input, clipboard, journal, manifest and export responsibilities to the target architecture.
- Identify code to retain, adapt or retire, including feature-dependent callers.
- Record the starting revision and the concrete source locations used by subsequent stages.

Deliverable: a compact working-state entry and concrete file mapping. Do not turn this stage into another general audit or redesign.

Source-level completion: the implementation starts from one identified baseline; planned paths below are mapped to actual source paths. No code completion or runtime claims are made.

## P01. Establish module and protocol boundaries

Work:
- Introduce the small workspace structure from the architecture: protocol, core, input-helper and desktop/UI packages, adapting existing Cargo structure only as needed.
- Keep helper dependencies free of GUI, video and database code.
- Define typed session/attempt, monitor, geometry, frame, request and stream identifiers; timestamp and uncertainty types; typed errors and result dispositions.
- Define lightweight helper IPC records and core input/context/frame/observation messages with bounded payloads.
- Define the immutable resolved capture configuration and M1 stub-profile/approval input.
- Define package artifact references and a narrow confirmed-save signal, without M2 file-service implementation.
- Separate common modules from cfg(target_os)-selected Windows modules and target-specific dependencies.
- Preserve existing dependency choices; use redb only where actual persistence is needed, not as a new M1 prerequisite.

Deliverable: concrete types, module APIs, Cargo manifests and composition roots used by subsequent code. No runtime plugin framework or empty service processes.

Source-level completion: dependencies are acyclic, Windows types do not escape portable APIs, and the helper does not depend on the full core/UI.

## P02. Implement time and recording lifecycle

Work:
- Implement QPC origin/frequency conversions using checked integer operations and UTC anchors.
- Preserve source versus receipt timestamps, coarse tick uncertainty and stream ordering semantics.
- Implement the dedicated session-controller thread and typed UI/control commands.
- Implement Starting, Recording, Paused, Stopping, Finished and Failed; pause reasons form a set.
- Implement immutable session configuration, start/countdown boundaries, admission epochs and idempotent Pause/Resume/Stop.
- Define local producer-prefix completion at lifecycle boundaries without a global per-event barrier.
- Wire status snapshots into the existing UI and supply the common clock/configuration to workers.
- Add protected control delivery so a full data queue cannot swallow Stop.

Deliverable: a single authoritative clock and lifecycle path.

Source-level completion: no competing Instant-based recording timeline or duplicate controller owns admission; all connected producers receive explicit boundaries.

## P03. Implement package IO and diagnostic foundations

Work:
- Create unique attempt directories and initial incomplete manifest using the target relative layout.
- Implement a writer per stream, bounded queues, receipt-order staging and seq assignment only to actual published rows.
- Implement out-of-queue loss counters and ordered gap publication.
- Implement normal buffered/OS flush requests and typed write/flush outcomes.
- Implement sole metadata-file ownership and the manifest update/publication path.
- Implement diagnostic snapshots, English messages, report writing and report-unavailable outcomes without captured secrets.
- Connect controller/worker errors to UI and package status without automatic recovery.
- Add a minimal read-only package interface for the application's later inspection/finalization logic; no exporter or server implementation.

Deliverable: package writing and error paths used by all capture producers.

Source-level completion: no second writer exists for a journal or manifest; a failed writer does not recursively generate unbounded health errors.

## P04. Implement native capture for the selected monitors

Work:
- Implement selected stable monitor identities, physical geometry, DPI awareness and per-monitor capture owners.
- Adapt WGC to recorder-owned native textures, short callbacks and immediate release of borrowed frames after ordered copying.
- Implement SDR and the specified HDR conversion path and color metadata.
- Implement four-frame history, frame/source identity, bounded retention accounting and immutable consumer leases.
- Implement normalized GPU thumbnail change scoring without full-resolution CPU reads for every frame.
- Implement the explicit prepublication boundary with the M1 policy; do not allocate the full future M3 privacy hold.
- Implement staging/readback ownership on the capture thread and the basic request/result path.
- Expose geometry epochs and resource-close/reconfigure operations for later lifecycle integration.

Deliverable: an owned native capture path per selected monitor.

Source-level completion: no downstream consumer can overwrite a retained texture, and no later image can masquerade as an earlier source frame.

## P05. Implement video slots and segmented recording

Work:
- Adapt hardware encoder selection and startup software 15 FPS selection; implement the specified startup self-check code without executing it.
- Keep still/capture pacing independent of encode FPS.
- Implement integer video slots, repeated-frame provenance, missing/skipped-slot outcomes and segment-local PTS mapping.
- Implement the specified H.264/no-B-frame/color/bitrate behavior and owned bounded sample lifetime.
- Separate encoder/mux IO from WGC collection; use encoded H.264 input to the muxer without a hidden second encoder.
- Implement 60-second rotation, actual IDR/header verification, bounded input while finalizing, and the retained boundary IDR.
- Implement normal segment drain, release, flush and publication.
- Retain/adapt existing frame-time consistency logic as product code for the per-segment layout; no verification run is scheduled here.
- Diagnose runtime encoder failure; do not add recreation/restart/fallback recovery.

Deliverable: per-monitor native-resolution segmented video with truthful frame index.

Source-level completion: source time, slot time and encoded PTS remain distinct; capture never waits for muxer Finalize.

## P06. Implement the input helper process

Work:
- Move native input collection into the small UI-less helper.
- Implement restricted inherited/shared-memory IPC, explicit record framing/bounds and parent-lifetime coupling.
- Implement the Raw Input message loop/buffered drain, hook-only callback loop, device arrival/removal and descriptor collection.
- Preserve callback receipt timestamps and source sequence before downstream delay.
- Implement startup readiness/failure reporting and controller forwarding of required display/session/settings notifications.
- Implement helper termination detection, blind/error reporting and ordinary cleanup; no automatic restart.
- Define deployment metadata needed for the named helper binary and signing/install packaging without making signed release production a development execution task.

Deliverable: helper and core receiver wired into the actual process entry points.

Source-level completion: no native callback performs file/GPU/network IO or waits for core processing; helper loss cannot leave UI falsely showing healthy recording.

## P07. Implement input matching and state

Work:
- Implement bounded Raw Input/hook matching, including ambiguous/unmatched evidence and motion coalescing.
- Implement origin precedence, device identity, separate event/receipt positions and unavailable reasons.
- Implement keyboard normalization, repeats, physical/logical mouse buttons, wheels and held-state updates.
- Separate private physical state, published state and control-chord consumption.
- Implement event sampling after matching, preserving held motion and first/last movement samples.
- Implement progress bounds that include unresolved matching, sampling and hotkey records.
- Implement recorder hotkey registration/validation code and command routing.
- Implement gap state invalidation and marked boundary synchronization without reconstructing missing physical actions.
- Define the early provisional-observation request construction point before matching/classification, plus its linkage to later publication/suppression. Actual routing to temporal selection and retention is completed in P10; do not claim before-frame preservation from this contract alone.

Deliverable: complete input evidence path through the core gate boundary.

Source-level completion: no arbitrary match is reported as certain; no full queue blocks input collection; provisional request IDs and resolution semantics are implemented. The declared capture-retention integration remains assigned to P10.

## P08. Implement context, UIA and text gating

Work:
- Implement foreground/window/display/layout/cursor observation and the required dedicated context loops.
- Implement per-frame/context geometry snapshots, cursor assets, approved identity handling and selected-monitor routing.
- Implement the dedicated COM/UIA worker, focused-element and pointer-target queries, time bounds and hung-app handling.
- Implement focus generations, bounded pending classification, stale-result rejection and context completion timing.
- Implement text inference/commit provenance, layout/dead-key/AltGr and IME rules.
- Implement password/possibly-sensitive field aggregation and suppression of per-key observation timing leaks.
- Resolve ordered input slots to permitted output without global stream reordering.
- Preserve optional clipboard behavior on its own worker, disabled by default; no clipboard read on journal/UI/capture threads.
- Route busy/save-related context through app-neutral value contracts; do not add production app adapters.

Deliverable: connected input/context publication with the M1 privacy scope.

Source-level completion: late UIA cannot classify a newer focus; sensitive buffered keys cannot escape through journals, still records or diagnostics.

## P09. Implement pen and applicable extended input

Work:
- Implement required pen HID collection registration and length-checked report parsing.
- Preserve report/sub-index timing, declared fields/ranges, signed values, pressure and optional Scan Time mapping.
- Link promoted pointer evidence conservatively without losing pen provenance.
- Report missing pressure/fields and unavailable modes explicitly; do not open a Wintab context.
- Integrate pen stroke state with held-sample retention and capture settings.
- Implement the PDF SHOULD touchpad/3D-mouse collection paths with bounded parsing and unavailable outcomes; any omitted SHOULD requires a stated reason, not silent removal.
- Leave optional MIDI outside the mandatory M1 path unless separately selected.

Deliverable: native pen and extended-device evidence integrated with input and manifest metadata.

Source-level completion: an absent measurement is not zero, duplicate promotion is not a second physical press, and unsupported devices do not produce invented data.

## P10. Implement live native stills

Work:
- Implement start, pre_motion, pointer_rest, pre_action, next, settled, observe, checkpoint and final trigger handling under the algorithm addendum.
- Keep checkpoint as confirmed-save input; a hotkey alone remains a request.
- Connect P07's early producer point to the actual capture owner before matching/UIA waits; implement mandatory early retention, adjusted before/seen selection and missing-calibration uncertainty. Trace this producer-to-retention path in source, not only the final post-classification trigger.
- On delayed action end, search bounded history for the earliest eligible next frame; detect lost first-frame evidence.
- Implement overlapping key/button actions, scroll bursts and shared per-monitor settled watchers with per-request deadlines.
- Implement bounded nonblocking readback and one initial PNG worker.
- Implement exact pixel/header BLAKE3, immutable image names, deduplication, logical observation references and rate/priority rules.
- Resolve still publication only after persistence; suppress sensitive provisional work without per-key drop leaks.
- Remove the production dependency on post-Stop H.264 decoding to obtain screenshots.

Deliverable: end-to-end event-to-native-PNG path.

Source-level completion: timing/cursor metadata cannot create duplicate content files, while distinct observations retain their identities; held images have explicit release paths.

## P11. Integrate boundaries, display changes and Stop

Work:
- Connect manual pause/resume, lock/sleep/secure-desktop boundaries and reason-set behavior.
- Implement synthetic closure of published held input and safe resume synchronization.
- Implement ordinary DPI/position updates, format/size/HDR changes and selected monitor disappearance/return.
- Retain original geometry on old pending work and prevent cross-epoch observation selection.
- Implement disk reserve checks, low-disk pause and explicit resume threshold.
- Integrate Stop admission closure, final-frame retention, producer-prefix completion, pending-classification resolution, queue drain and flush ordering.
- Drain only pre-Stop-confirmed final-save settled images; cancel unconfirmed future watchers explicitly.
- Finalize required segments and journals, then manifest; honor the specified shorter suspend/shutdown waiting budgets.
- Preserve exclusive ownership when an OS call remains blocked beyond controller waiting; no replacement writer or false cleanup success.
- Keep runtime failures diagnostic-only as agreed.

Deliverable: one connected lifecycle across every capture producer and output owner.

Source-level completion: Stop is not merely a UI flag, pause cannot be accidentally cleared by another reason, and no owner can continue normal admission after closure.

## P12. Finish minimal UI and startup integration

Work:
- Bind monitor selection, directory picker, capture settings and controls to the new controller/configuration.
- Implement startup progress/readiness, the countdown, active/paused/stopping/failed status and report-path display.
- Wire required startup capability checks and clear explanations into Start admission.
- Keep user selections independent of mutable worker internals.
- Make UI command submission nonblocking and repeated commands idempotent.
- Add the platform window-lifecycle boundary needed for future own-window exclusion; full M3 exclusion behavior remains deferred.
- Preserve current useful UI behavior; do not introduce the full task/review/upload interface early.
- Use redb only for actual required/existing persistence; do not add the M2 task catalog.

Deliverable: a minimal user interface connected to all required M1 operations.

Source-level completion: no obsolete start/stop/recording path remains reachable through a UI control, command-line entry or feature branch.

## P13. Complete package integration and documentation

Work:
- Finish minimal manifest inventory: clocks, actual devices/displays, profile/modes, geometry/segments, stream counts, gaps and completion/failure.
- Reconcile all artifact-reference producers with their package readers/writers.
- Implement the source-side counters and diagnostic metadata needed by later measurements, without benchmark tools or test execution.
- Remove obsolete duplicate recording/export paths only after tracing their callers and supported modes.
- Retain optional/unknown distinctions, including unavailable calibration, pen support and stub approval status.
- Update the project documentation and entry-point descriptions to match the developed M1 code and explicitly mark superseded historical behavior.
- Record which M1 implementation items are developed, any unresolved source issues, and separately that compilation/execution/hardware acceptance have not been performed under this plan.
- Complete independent source review and corrections.

Deliverable: a coherent M1 source tree and matching documentation, with an implementation completion report.

Source-level completion: all mandatory scoped features have a production entry-to-output path; no TODO/fake success substitutes for required behavior. No claim that the program builds, runs or meets measured targets is implied.

## 4. Coverage of the agreed M1 scope

| Scope item | Development stages |
| --- | --- |
| Platform and lifecycle (M1-01) | P01-P02, P11-P12 |
| Clock and schemas (M1-02) | P01-P03, P13 |
| Monitor capture and image processing (M1-03) | P04, P11 |
| Video and segments (M1-04) | P05, P11 |
| Helper and input (M1-05) | P06-P08 |
| Pen (M1-06) | P09 |
| Context/focus/text (M1-07) | P08 |
| Native stills (M1-08) | P10 |
| Journals/package/diagnostics (M1-09) | P03, P11, P13 |
| In-product diagnostic/measurement support (M1-10 subset) | P02-P13 as needed |
| Development verification tools, tests and hardware acceptance | Not scheduled; separate work |

The M1 specification's verification-support item is not marked fully complete merely because counters exist. Test/tool development and execution, if required for that item, are explicitly outside this development-only plan.

## 5. Completion record

Maintain one concise progress table with columns:
stage, source baseline, state (not_started/in_progress/developed/blocked), changed files, implemented contracts, critic findings, unresolved limitations.

A blocked stage identifies a concrete unresolved requirement or missing source input. Do not mark it blocked solely because compilation or a hardware bench was not run: those activities are outside this plan. Conversely, do not use their exclusion to conceal a known source defect.

The final outcome is M1 development completed with separately stated validation status. Hardware acceptance and release readiness remain independent decisions.

