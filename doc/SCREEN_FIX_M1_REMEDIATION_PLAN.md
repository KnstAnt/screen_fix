# M1 remediation plan

Date: 2026-11-17 (Europe/Moscow)
Project: screen_fix / Intercall Recorder, milestone M1
Input: `review/M1_conformance_review_2026-11-17.md` (static conformance review) and the current working tree as the baseline.

## 1. Purpose, authority and status

This plan fixes the confirmed defects from the M1 static conformance review. It also completes the M1-08 and M1-10 verification obligations. Source priority follows the project rules: direct user decisions, then goals (`target.txt`), then roadmap (PDF §17, M1 specification addendum), then contracts (PDF, addenda), then algorithms, then code.

Status of the evidence:
- Every review finding was re-checked by reading the code. Section 3 records the verdicts. Rejected and corrected findings are listed in section 4.
- Nothing was compiled or executed. The project rules forbid build, typecheck, formatter, tests and application runs without separate user authorization. Each stage states what a reviewer can confirm by reading code and what needs execution.
- Stages that depend on an open documentation conflict are marked **blocked by D-n**. Section 5 lists the decisions. Unblocked stages can start immediately, in the dependency order of section 7.

Out of scope: M2–M6 features, automatic recovery (A2), and refactoring that no listed defect requires.

## 2. Systemic causes

The review lists about 100 findings, but they reduce to five systemic causes. The plan fixes each cause once rather than patching every symptom.

| Cause | Symptoms (examples) | Systemic fix |
| --- | --- | --- |
| **C-A. No uniform terminal-outcome accounting.** Owners discard items they cannot publish. They often increment a private counter that nobody reads. The only journal-level gap row has one fixed reason, `queue_overflow`. | I-1, I-2, I-4, the input/context/stills/elevated bullets in review §2.1, unread `ProvisionalRequests` counters, health rows only at Stop | One loss ledger in the stream ingress, plus one terminal-outcome vocabulary used by every owner (§6.1, stage A1) |
| **C-B. No consistent rule for when a condition fails the attempt.** Single malformed packets, transient stalls, display races, sleep and provider timeouts end the attempt. Other genuine losses go unrecorded. | S-1, E-1, E-3, the review §2.2 table | One fault classification with an enumerated condition table, enforced by the controller. Classes: attempt failure, required activity loss, auxiliary activity loss and recorded loss (§6.2, stage A2) |
| **C-C. Matching outcomes are treated as publication decisions.** Correlation quality (Ambiguous, MissingScan, Discontinuous) decides whether a transition exists at all. | I-1, I-2, I-3 | Correlation becomes evidence metadata. Publication depends on approval and privacy only (stages I1–I3) |
| **C-D. Capacities and deadlines are fixed constants, not derived from the configured rate or from upstream bounds.** | V-1, E-3 (zero margin), hotkey 250 ms, producer 250 ms | Derive bounds from the effective FPS, the provider timeouts and the documented stall budgets; record the effective settings (stages V1, E2, I5, S1) |
| **C-E. Serialized schemas drifted from the PDF without a versioned description.** | review §2.3 | A decision (D-1), then one schema-conformance stage plus a versioned schema document (stage R1) |

## 3. Verified baseline

Each finding was checked against the code; the result is shown in brackets. Line references were corrected where the review was off. Severity is restated where the verification changed it.

### 3.1 Input
- **I-1 [confirmed].**
  - Any raw motion is compatible with any hook motion within ±20 ms (`core/src/capture/input/matching.rs:14,636`). `motion()` requires each raw candidate to have exactly one hook candidate (`:558-579`), so dense motion becomes Ambiguous. Ambiguity spreads to neighbouring candidates (`:387-392`).
  - Wheel events with an equal value behave the same way (`:635`, `:499`).
  - Ambiguous records fail `approved_receipt` (`core/src/platform/windows/input/gate.rs:983`). They are retired as Suppressed into `ProvisionalRequests.suppressed` (`receiver.rs:949`), which is never read.
- **I-2 [partially confirmed].**
  - Closing calls `finish(cutoff-1)`, and an item is certified only if `t+40 ms` (or `t+60 ms` for motion) is below that bound (`matching.rs:371-378,488-490`; `receiver.rs:455-468`). So roughly the last 40–60 ms before every Pause and Stop becomes Discontinuous and is suppressed. Items within ±20 ms of a gap or barrier are lost the same way.
  - A false `repeat:true` appears only when an Ambiguous or MissingScan `key_up` is suppressed mid-session. Boundaries close published held state correctly.
- **I-3 [partially confirmed].**
  - Injected hooks never match (`matching.rs:618-627,641-647`). Hook packets carry no foreground proof (`native-observation/src/input/hooks.rs:417`), so hook-only records are always suppressed. `injector_hint` is computed (`evidence.rs:185-189`) but never serialized.
  - A raw-only SendInput packet is still published, but with `origin:"unknown"`. Injected input is therefore never labelled `injected`, except a matched VK_PACKET.
- **I-4 [confirmed].**
  - `input_continuity_gap{reason, admission_epoch}` has no count or range. Its `recv_t_us` is the last published time (`gate.rs:893-903`).
  - `InputGap` carries ranges, but they end in the unread `private_gaps` (`receiver.rs:959-965`).
- **I-5 [confirmed for non-modifier keys].**
  - The first autorepeat after a boundary is published as `repeat:false` and requests a pre-action still (`held.rs:287-291`; `gate.rs:544-551,572`).
  - A `key_up` without a published down is suppressed (`gate.rs:526-531`). Buttons in the same situation get `press_unavailable`.
- **Review §2.1, input losses [confirmed].**
  - Opening-resync drops have no counter and no bound (`receiver.rs:138-140,333-347`).
  - Raw packets are dropped for any non-ordinary foreground, not only higher integrity (`receiver.rs:128-130`).
  - `ControlDisposition::Uncertain` is suppressed like a Recorder key (`gate.rs:343-345,415-422`).
  - Unknown devices lose their identity. The helper assigns IDs without sending `DeviceArrived` (`native-observation/src/input/device.rs:234-250`). HID reports from such devices are dropped, and only a manifest flag records it (`core/src/platform/windows/input/devices.rs:66-90`).
- **Review §2.1, text [partially confirmed].**
  - `TextStatus` has no `unknown` (`text.rs:40-47`).
  - Late, stale or overflowing TextEdit commits are dropped silently (`gate.rs:283-337`). The exception is commits behind the published prefix, which become a counted gap (`:309-315`).
- **Review §2.2, input [confirmed].**
  - Optional collections (touchpad `(0x0D,5)` and 3D mouse `(1,8)`) are always registered, and any registration mismatch is fatal (`raw.rs:75,96-149,242-249`). All Raw Input traffic shares one producer and loss slot.
  - A producer stall over 250 ms retires the helper for the rest of the attempt (`receiver.rs:27,514-521`; `input.rs:304-308`).
  - A malformed packet or a `PacketError` kills the helper (`raw.rs:169-181,411-451`; `core/capture/input/src/windows.rs:119-179`).
  - A hotkey progress gap over 250 ms disables all hotkeys permanently, including Stop (`core/src/session/windows.rs:1476-1529`).
- **Review §2.3, input schema [partially confirmed].**
  - `modifiers`/`modifiers_known` are bitmasks, and `scan` is an object.
  - Pointer data is nested in `pointer{}`, and the held state is the state before the event.
  - There is no `input_health` kind.
  - Blind rows go to `context.jsonl`. A helper-failure blind is written only at Stop, starting at detection time. Pause blinds are written at resume or Stop.
- **Review §2.4, input [partially confirmed].**
  - Blocking device IO runs on the Raw Input thread (`device.rs:59-206`), and process queries run on every WM_INPUT (`raw.rs:166,287-353`).
  - The governed path disables the buffered drain (`raw_loop.rs:246`).
  - Hooks use a `thread_local RefCell`, but only with shared `borrow()`, and `catch_unwind` is present.
- **Review §3, input and helper [confirmed]:**
  - `LowLevelHooksTimeout` is read only as REG_SZ.
  - Unsigned HID values are lost on unexpected scaled-value statuses.
  - The first HID Scan Time uses the receipt time as source time.
  - Absolute mouse raw x/y and flags are not serialized.
  - `&mut` is taken over cross-process shared memory.
  - `InputHelper::drop` waits INFINITE (bounded in practice by the job kill).
  - Motion envelopes consume EarlyRows slots.

### 3.2 Video, session, package
- **V-1 [confirmed].**
  - `MAX_SEGMENT_PICTURES = 61*30+2` and `MAX_STAGED_SLOTS` are independent of FPS (`video.rs:29-30`). At 60 FPS, staging overflows after about 30.6 s, and that monitor's video becomes unavailable for the rest of the attempt.
  - `EVIDENCE_SLOTS = 2048` only affects the `late` flag (it can give a false negative).
- **V-2 [confirmed, not silent].** A frame whose source time is in slot k but that arrives after slot k closes is discarded (`video.rs:180-186,417`). The slot repeats the old frame with `late:true`.
- **V-3 [confirmed].**
  - Full software decode verification runs synchronously inside `output()` on the video worker (`video.rs:562`; `mp4.rs:96-110,144-199`). The capture queue `sync_channel(2)` overflows during it, and slots get `QueuePressure`.
  - The watchdog fires only in the async-MFT case.
- **S-1 [confirmed, conditional].** Suspend sets `segment_deadline = suspend + 2 s`. Missing it fails the attempt (`session/windows.rs:1228-1233,1273-1289`; `mp4.rs:90`; `platform/windows.rs:237-243`).
- **Review §2.1, video [partially confirmed].** Frames that arrive before the timeline is republished after Resume are skipped without a counter (`monitor.rs:351-352,603-613`). The slots get a misleading `BeforeFirstFrame` reason.
- **Review §2.2, video and session [confirmed].**
  - Any `display::enumerate` failure stops the attempt (`display.rs:118,162-178`; `monitor.rs:222`).
  - An unsupported-token approved foreground fails the attempt even in a standard epoch (`selector.rs:106-115`; `session/windows.rs:589-609`).
  - HW candidate failure causes are discarded (`encoder.rs:66-77`).
- **Review §2.3, frames and manifest [confirmed].**
  - Frame `recv_t_us` is the publication time; the capture receipt is kept separately as `capture_recv_t_us`.
  - The manifest has no `health` block and no images/cursors inventory.
- **Review §2.4, session [confirmed].**
  - Workers mutate the shared `Facts` (`session/windows.rs:344-350,518,599,636,713,772,923,952`).
  - The state machine is spread over several latches.
  - On reconfigure, the encoder self-check uses the capture device's immediate context, serialized only by multithread protection.
  - The power callback takes a Mutex, and neither environment callback has `catch_unwind`.
- **Review §3, session and video [confirmed]:**
  - The first cause is lost when the Stop deadline expires.
  - Non-elevated admitted failures look like preflight errors in the UI, and `close_after_recording` is lost.
  - The indicator diagnostic is lost on error.
  - `check_deadline` runs after a successful `MoveFileExW`.
  - `CaptureError::backend` always reports `WindowsCapture`.
  - A DPI or position change resets the repeat source.
  - HDR without an SDR white level is fatal.
  - Bitrate metadata names are misleading.
  - The SW path does readback at capture pace.
  - Lock during the countdown is invisible.
  - Health rows can get `recv_t_us: null`.

### 3.3 Stills and package
- **T-1 [confirmed].** No test exists anywhere, and `validate_v5_artifacts` is never called. It covers gate 1 and the gate 2 index mapping only, not gates 3 and 4.
- **T-2 [partially confirmed].** `route_fixture` is live (`input.rs:276,1190`), but nothing calls `SessionHandle::confirm_fixture_save` (`session.rs:388`).
- **Review §2.1, stills [confirmed].**
  - The PNG inbox rejects a job on `try_lock` contention with reason `capacity` (`still_png.rs:102-105`).
  - Several drop rows carry the wrong reason (`stills.rs:542-560,914`).
  - PNG write failures produce no health row, and the UI still reports "Recording complete" (`platform/windows.rs:305`).
  - Cursor asset losses look like "no asset needed" (`cursor.rs:66,121-127`).
- **Review §2.4 C2 [confirmed].** One readback, one CPU buffer and one PNG job are created per logical slot. The staging texture is recreated per job.
- **Review §3, stills [confirmed, unless noted]:**
  - Each scroll tick without a device opens its own burst (bounded; the review's "unbounded" was overstated).
  - Private provisional candidates hold up to all three extra holds and are excluded from `reclaim`.
  - `start` ignores `last_evicted`.
  - `expire_drain` leaves PNG jobs running.
  - The reader checks `event_ref` by sequence range only.

### 3.4 Context
- **C-1 [confirmed].** `editable()` (`native-observation/src/uia.rs:510-541`) behaves as follows:
  - It returns `None` when neither the Value nor the Text pattern is present and the control type is not in a fixed list. The result is a redacted visit.
  - It returns `Some(false)` for the listed types (Button, Menu, ListItem and others).
  - It honours `ValueIsReadOnly=true` without consulting a writable Text pattern. This is a privacy-relevant under-redaction.
  - Classification (`uia.rs:268-273`) also treats `HasKeyboardFocus` being false or unsupported, and an unknown `IsPassword`, as PossiblySensitive.
- **Review §2.1, context [confirmed]:**
  - TextEdit commit losses are silent: slot overflow, more than 1024 UTF-16 units, more than 8 parts, invalid UTF-16, and the `future_text` cap.
  - Clipboard, cursor-completion and cursor-asset queue losses are silent.
  - `context_notifications_gap` has a count but no interval.
  - The WinEvent callback does not filter `GA_ROOT`. Child-window events consume ring capacity, and overflow then drops and counts new events.
- **Review §2.2, context [confirmed].** A canonicalization failure, or a resolution pending longer than 2 s, makes `Context::poll` return Err and ends the input loop (`context/identity.rs:135-149`; `context.rs:313`; `input.rs:267`).
- **Review §2.3, context schema [confirmed].** The field names differ from the PDF example rows.
- **Review §3, context [confirmed, unless noted]:**
  - A Ghost window is not approved.
  - LOCATIONCHANGE has no trailing re-read.
  - A stuck UIA worker is not diagnosed.
  - ES_PASSWORD is checked on any window class (fails safe).
  - UIA_E_TIMEOUT is written as `unavailable`.
  - Without a focus snapshot, IME state becomes `not_ime`.
  - There is no `hwndActive` fallback.
  - MINIMIZE, SWITCH and IME events produce no records.
  - A click during a pending pointer query gets no read.
  - Clipboard data over 64 KiB is skipped without a record.
  - CTX-12 has no tablet driver version or mode.
  - The `approved:false` row on a metadata race is a narrow race, not a general failure.
  - The total UIA deadline can be exceeded.
  - The `transmute` in `hung.rs` has no SAFETY note.

### 3.5 Elevated
- **E-1 [confirmed].** The Recorder's own `maintain`, `activate`, Armed, held and ResyncAccepted checks return `TargetChanged` on any foreground or focus-revision change. This happens whether or not Start has occurred, and the result is a failed attempt (`visit.rs:183-208,313-314,339-340,437-445,496`).
- **E-2 [confirmed].** While the classification is `None` or `Unknown`, helper Input, Progress and non-device Gap records are discarded with no record and no resync (`selector.rs:359-366,937-939`; `input.rs:253-258`; `receiver.rs:200-207`).
- **E-3 [confirmed].**
  - The UIA and IME slot deadlines equal the provider's own timeouts, so there is zero margin.
  - Any overrun ends the collector session: stop and cancel, then terminate after 750 ms (`collector.rs:127-177`; `runtime/context.rs:336,523-527,628-634`).
  - The Recorder then fails the attempt.
- **Review §2.1, elevated [partially confirmed]:**
  - A scope-serial reset erases the pending loss detail. The interval becomes uncertified, not silently certified.
  - A gap that crosses the resync point is dropped, but in practice this is rare.
  - Rejected text-edit commits are dropped without a count.
- **Review §2.2, elevated [partially confirmed].** A data-lane write over 100 ms tears down the connection (`owner.rs:30,161,193`). This happens only when the Recorder stays stalled.
- **Review §2.4, elevated [confirmed]:**
  - Token verification runs per WM_INPUT and per egress record (`runtime/input.rs:50`; `grant.rs:109-120,173-180,399`).
  - The governed path has no buffered drain.
- **Review §3, elevated [confirmed or partially]:**
  - The collector does not independently bind the approval key to the image.
  - Deferred checkpoints are cleared at Pause/Stop without a row.
  - The host is dropped without `Control::Stop` after an admitted failure.
  - The lease can expire during a long pause-closing Stop.
  - The Armed held result is discarded.
  - UAC dispatch/return and withdrawn launches are not recorded.
  - Returning to the standard source does not recheck hook health.
  - The bootstrap pipe grants `FA` to the logon SID (`pipe/security.rs:37,87-90`).
  - The Arm QPC is taken before hook registration.
  - `Secret` is moved by value.
  - `ticks()` silently assumes `LEASE = 1 s` (`lifetime.rs:37,97-101`).
  - Any pause in an elevated epoch labels its blind interval `elevation_transition`.

### 3.6 Dead code [confirmed]
- `ObservationTrigger`, `ObservationRequest`, `ObservationSelection` and `ObservationRequestError` (`core/src/capture/observation.rs`).
- `SegmentPts100ns`, `VideoSlot` and `SlotDisposition` (`core/src/capture/frame.rs:90-106`).
- `ForegroundObservation`, `ApprovalStatus`, `FocusClassification` and `MAX_WINDOW_TITLE_BYTES` (`core/src/capture/context.rs`).
- `InputAttachment` and `InputControl` (`core/capture/input/src/lib.rs`).
- `CoreToHelper::EnvironmentChanged` and `Stop` (`protocol/src/ipc.rs:69-73`).
- The `video == None` branches in `monitor.rs`.
- `ConfirmationSource::ApplicationNotification` and the unused `SaveSignalError` variants (`core/src/capture/save.rs`).
- The README claim that `Cargo.lock` is absent is false: the file is tracked.

## 4. Rejected and corrected findings

| Finding | Verdict | Reason |
| --- | --- | --- |
| P-1 (frames gap row leads to Failed) | **Rejected** | The frames journal is written only through the blocking `submit_verified`. It never records a loss, so `frames.dropped` is always 0. Video losses are slot rows with a reason. No plan change. |
| stills `frame_obs_id` ambiguous across capture epochs | **Rejected** | `FrameId.sequence` is one per-monitor counter that never resets. Combined with `monitor` it is unique, and the full `frame.id` is also serialized. |
| I-2 false `repeat:true` after Pause/Stop/gap | **Corrected** | Boundaries close published held state with synthetic ups. A false repeat follows only a mid-session suppressed Ambiguous or MissingScan `key_up`. That case is fixed by stage I1. |
| I-3 "injected input is practically not published" | **Corrected** | Raw-only injected packets are published, with `origin:"unknown"`. The defects are that the injected origin and hints are lost and hook-only events are suppressed (stage I3). |
| I-1 "no counter" | **Corrected** | A counter exists but is never read. Covered by stage A1. |
| WinEvent callback with Mutex and without `catch_unwind` (context) | **Rejected for context code** | `native-observation/src/events.rs` uses `catch_unwind` and has no lock. The defect exists only in `session/windows/environment.rs` (stage S4). |
| `thread_local RefCell` in hooks causes callback risk | **Corrected (low)** | Callbacks only borrow, and `catch_unwind` is present. It is still an ARCH-2 rule deviation (stage I6). |
| WinEvent `GA_ROOT` events evict top-level events | **Corrected** | Nothing is evicted. New events are dropped and counted, which produces a gap and an invalidation. The capacity waste remains (stage C3). |
| Scroll without a device gives unbounded PNG load | **Corrected** | The load is bounded by the scroll cap of 32, the queues, the single readback and dedup. Excess becomes drop rows. Burst splitting remains a defect (stage P2). |
| Elevated calibration labelled `missing_calibration` | **Rejected (moot)** | M1 rejects any non-empty calibration list (`platform/mod.rs:89-95`), so the label is accurate. |
| `manifest.pending.json` blocks a second publish | **Rejected** | No second publish exists, because `finish_recording` consumes the owner. The leftover pending file is not a published artifact. No change. |
| Published `.mp4` renamed back to `.part` | **Corrected** | This is intentional rollback. Stage V3 publishes only after verification, which removes the rollback path. |
| HW→SW encoder switch on resize is silent and forbidden | **Corrected** | It is recorded (`degraded`, `fps`, per-segment `software`). It is a capability selection for a new format (VID-9, VID-6), not A2 fault recovery. Only the lost causes are a defect (stage V4). |
| Hook progress comes from the thread loop, not the hook | **Rejected** | Progress means "every earlier record is enqueued", which the loop provides correctly. Silent hook removal is HookHealth's job (stage I5, decision D-2). |
| Global statics (`RECORDING_OWNED`, `REPORT_SERIAL`, `DESKTOP_SIGNALS`) | **Corrected** | All are justified: a process-wide one-recording latch, a unique report name, and callback context without a user-data pointer (ARCH-2 permits preallocated statics). Only the documented exception is missing (stage D1). |
| `InputHelper::drop` waits INFINITE | **Corrected (low)** | The job kill bounds it in practice. A bounded wait is still added for A2 conformity (stage I6). |
| Data-lane write over 100 ms | **Corrected** | This needs a sustained Recorder stall. It is still a contract deviation (stage E3). |
| Gap crossing the resync point | **Corrected** | The pattern is real but needs a native overflow inside a narrow window (stage E3). |
| Review §5.2 helper role contradiction | **Rejected** | The M1 architecture addendum explicitly places matching in the Recorder Receiver. The observable INP-9 behaviour (control keys never journaled) is unchanged. |
| Review §5.9 `images/…` vs `raw/images/…` | **Corrected** | The PDF URI is relative to `raw/` (where `stills.jsonl` lives), and the README URI is attempt-relative. The base differs; there is no contradiction. The row shape is part of D-1. |
| Review §5.10 `checksums.sha256` in M1 | **Corrected: not an M1 defect** | PDF §17 lists M1 scope without checksums and M1 acceptance uses gates 1–4 only. Gate 8 first appears in M2 acceptance (T18). The roadmap outranks contracts. A documentation clarification only (stage D1). |
| Review §5.13 eviction order vs REL-2 drop-newest | **Rejected** | REL-2 sends still jobs to IMG-3. Drop-newest applies to other queues. The code drops the newest of equal priority, consistent with both. |
| Review §5.14 PNG pool vs one worker | **Rejected** | C2 and M1-08 explicitly amend to one worker. ALG §5.2 supplies the numeric bounds. |
| Review §5.16 controller thread | **Rejected** | The ALG allows ("can share"), the architecture requires a dedicated thread, and the code has one. |
| Review §5.17 WGC time formula | **Rejected** | ALG §3.1 defers WGC mapping to CLK-3, and the code follows CLK-3. |
| Review §5.19 "no JSON rows cross the wire" | **Corrected** | The payloads are typed, bounded serde values inside the fixed binary header. "JSON rows" means journal rows. Clarification only (stage D1). |
| Review §5.19 pending grant vs sequential | **Rejected** | §3.2 sets a maximum ("at most one pending grant"), and a sequential implementation satisfies it. |
| Review §5.19 readiness budget per re-preparation | **Corrected** | The code gives each preparation a fresh 5 s. Doc clarification only (stage D1). |
| Review §5.20, §5.21 | **Corrected** | The architecture explicitly separates the context loops. "Consent" in M1-01 is the collector's UAC consent, because the PDF §14 consent box is M4. Clarifications only. |
| Review §5.15 F12 reserved | **Not a code defect** | The README already tells the user to choose another key. Losing hotkeys during recording is a defect, handled by stage S1. |
| INP-6 stroke-tool mode from the profile | **Not an M1 defect** | Stroke tools come from pack profiles, which arrive in M2. M1 keeps the held-button rule. |

## 5. Decisions required from the user

Each decision lists the conflicting sources, the options, the consequences and a recommendation. Stages blocked by a decision are marked in section 7. When a decision is made, its documentation change goes into stage D1.

**Decisions taken (user, 2026-11-17).** The user accepted the recommendation for every decision. The "blocked by D-n" marks in section 7 are resolved accordingly, and those stages follow the chosen option:

| Decision | Chosen option |
| --- | --- |
| D-1 | A with A-v6only: PDF field names, package format v6, the reader opens v6 only |
| D-2 | B: detection only (blind interval and `input_health`); no re-registration or hook reinstallation. Stage I8 is dropped |
| D-3 | A: suppression with counted suppression intervals, recorded as an explicit amendment |
| D-4 | a1, b1, c1, and d1 combined with d2 |
| D-5 | A: bounded 2 s wait, health row, finalization continues after wake |
| D-6 | A: interval rule |
| D-7 | A: hook-anchored association |
| D-8 | A: a target change is a transition; an ineligible approved foreground is suppressed |
| D-9 | A: independent canonicalization in the collector |
| D-10 | A: source-time watermark with bounded grace |

**Verification authorization (user, 2026-11-17).** `cargo check` (typecheck and compilation without running) is authorized for implementation and review. Running tests, the formatter, linters beyond `cargo check`, and the application remain unauthorized. Tests written under T1–T3 are compiled by `cargo check --all-targets` but not executed. Stage T4 stays pending.

### D-1. Journal and manifest schema conformance and versioning
- **Sources.**
  - ALG §3.2: "The exact serialized field names follow the PDF; any metadata extensions are versioned and documented."
  - The PDF gives only example rows for `input.jsonl`, `context.jsonl`, `stills.jsonl` and `frames.jsonl`.
  - The README documents the current shapes: bitmask `modifiers`, object `scan`, nested `pointer{}`, `foreground_window`, `pointer_element`, separate before and seen still rows, `raw/images/…` URIs, frame `recv_t_us` = publication time with a separate `capture_recv_t_us`, and blind rows in `context.jsonl`.
  - CLK-2 defines `recv_t_us` as "when the recorder received it". The PDF frames example has `recv_t_us` 1.6 ms after `source_t_us`.
- **Options.**
  - **A. Conform to the PDF where a PDF example or requirement names a field.** Keep all other current fields as documented, versioned extensions.
    - Input: `mods` as a list of side names, integer `scan` (with the E0/E1 prefix folded per INP-2), top-level `pos`, `pos_recv`, `monitor`, `held` (after the event), `delta`, `coalesced`, `warp`. `blind` and `input_health` go in `input.jsonl`.
    - Context: `foreground`, `pid`, `exe`, `rect`, `focus.uia{control:<name>, …}`, `"timeout"`, `ime` inside `focus`, `element`.
    - Stills: one row per trigger with `file`, `seen_file`, `frame_obs_id` and `seen_obs_id`, with `file` relative to `raw/`.
    - Frames: `recv_t_us` = the capture receipt of the selected frame, with the per-stream merge in §6.5.
    - Bump the package format to v6.
  - **B. Keep the current shapes** and record an explicit schema amendment in the M1 specification addendum plus a versioned schema document.
  - **C. Dual-write** PDF names alongside the current fields.
- **Sub-option for A: reader compatibility.**
  - **A-v6only.** The reader opens v6 only.
  - **A-v5read.** The reader opens v6 and also reads v5 packages through a separate, read-only v5 schema path. Gate checks (T1) run on both.
- **Consequences.**
  - A matches the approved contract and the M4 exporters' expectations. It touches the gate, context, stills and frames serializers, the reader and the README.
    - Under A-v6only, packages recorded before the change no longer open.
    - A-v5read keeps them inspectable but adds a second schema path to the reader, which must be maintained and tested.
  - B is cheapest, but formally changes an approved contract and leaves later milestones mapping two vocabularies.
  - C doubles row size and creates two sources of truth.
- **Recommendation: A with A-v6only**, because no M1 package has been released or accepted. Choose A-v5read if existing development recordings must stay readable. Stage R1 implements it after the loss and health work, so the new rows are designed once. Placing `blind` and `input_health` in `input.jsonl` also simplifies the gate 3 check.

### D-2. Raw Input re-registration and hook reinstallation
- **Sources.**
  - PDF INP-1: every 5 s check `GetRegisteredRawInputDevices`; "re-register and write an input_health record".
  - PDF INP-12: "A watchdog reinstalls a hook … and writes an input_health record".
  - ALG §6.1: "Automatic hook reinstallation, registration repair after loss and helper restart are not implemented as hidden M1 recovery."
  - Spec addendum §1 lets addenda override the PDF only by explicit amendment. A2 explicitly defers only the helper restart.
- **Options.**
  - **A. Implement re-registration and hook reinstallation** in the helper. Each is recorded as an `input_health` row and a blind interval covering the loss.
  - **B. Detection only.** Record a blind interval and an `input_health` row; the affected stream remains lost (hook-dependent fields become null with a reason) until Stop. Record this as an explicit amendment.
- **Consequences.**
  - A keeps long sessions usable. It is small and deterministic (re-register the same collections; reinstall the same hook on the same thread), but it is repair logic the ALG excluded.
  - B is simpler and consistent with A2, but one external interference degrades input until Stop.
- **Recommendation: B** for M1, with the amendment written into the spec addendum. Detection, blind intervals and `input_health` are planned in stage I5 either way. Stage I8 (repair) exists only if the user chooses A.

### D-3. Unapproved foreground: suppression or pause
- **Sources.**
  - PDF CTX-6 and PRIV-2: pause with reason `non_approved_app` or `always_pause_window`, a `late_pause` record, and pause/resume records carrying the slot range.
  - ALG §4 item 5, SPEC §1 and ELEVATED §4.2: "Unknown or unapproved foreground stays suppressed."
  - The architecture addendum §7.3 assigns the unified pause-cutoff policy and late-pause correction to M3. No A-level amendment exists.
- **Options.**
  - **A. Keep suppression in M1** and record it as an explicit amendment. Make each suppressed interval observable: one `foreground` `approved:false` row (already written), suppressed-input counts per interval in the loss ledger (counts only, no per-key timing), and suppression intervals in the manifest health.
  - **B. Implement PDF pause semantics now:**
    - auto-pause and auto-resume on every unapproved switch;
    - new segments per switch;
    - the `non_approved_app` and `always_pause_window` reasons;
    - `late_pause` without the 200 ms hold.
- **Consequences.**
  - A matches every M1 document except the PDF and avoids a segment per Alt+Tab. Gate 4 then has no pause intervals for these periods, and suppression is checked instead.
  - B is a larger controller and video change. Its `late_pause` cannot be honoured without the M3 hold.
- **Recommendation: A.** Stage C5 is blocked by this decision. It implements the A accounting or, under B, the pause path.

### D-4. Focus classification for undetermined editability
D-4 contains four sub-questions. Each one changes approved redaction behaviour in one direction or the other, so none is implemented without a decision.

| Sub-question | Case | Current code |
| --- | --- | --- |
| D-4a | Successful read, IsKeyboardFocusable known true, neither the Value nor the Text pattern present | Listed control types → `false` (keys kept); all others → redacted visit |
| D-4b | A pattern is present but its read-only attribute is unavailable (the property or `GetAttributeValue` fails or is unsupported), IsPassword known false | Redacted visit |
| D-4c | The Text range reports a mixed read-only attribute, IsPassword known false | Redacted visit |
| D-4d | `HasKeyboardFocus` false or unsupported on the element returned by the focused-element read, while the native focus snapshot confirms the same window | Redacted visit |

- **Sources.**
  - PDF CTX-9: "`editable` is true when IsKeyboardFocusable is true and either the Value pattern's IsReadOnly or the Text pattern … is false; a mixed or unavailable attribute gives `editable: null` (unknown, not typing)".
  - The PDF example row: `{"control":"Pane","editable":false}`. The PDF note says a bare G in Blender is not typing (T11).
  - PDF INP-4 redacts keys only "after a read that timed out or failed in a field that may take text".
  - ALG §7.1:
    - "Password or unresolved possibly editable field: aggregate one redacted visit".
    - "Known non-editable field: preserve keys as non-typing".
    - "Conflicting non-sensitive evidence: typing unknown".
  - Current code: no pattern and a type outside the list → redacted. Listed types → `false`.
  - **The conflict.** The PDF CTX-9 reads `editable:null` as "unknown, not typing", meaning keys are preserved. ALG §7.1 redacts an "unresolved possibly editable field" and keeps "typing unknown" only for "conflicting non-sensitive evidence". Neither source says which null cases count as unresolved and which count as conflicting.
- **Options for D-4a.**
  - **a1.** `editable:false`; keys preserved as non-typing. Applies to every control type, with no type list.
  - **a2.** `editable:null`; keys published with `typing:"unknown"`.
  - **a3.** Keep the current behaviour (type list, redaction otherwise).
- **Options for D-4b.**
  - **b1.** Unresolved: redacted visit (current behaviour).
  - **b2.** `editable:null`, keys published with `typing:"unknown"`.
- **Options for D-4c.**
  - **c1.** Conflicting evidence: `editable:null`, keys published with `typing:"unknown"`.
  - **c2.** Redacted visit (current behaviour).
- **Options for D-4d.**
  - **d1.** False: the element is not the keyboard target, so the read is stale. Re-issue the request; if the classification wait expires, redact (current outcome, with an explicit retry).
  - **d2.** Unsupported: treat the read as resolved when the element came from the focused-element query and the native snapshot confirms the window.
  - **d3.** Keep the current behaviour for both cases.
- **Consequences.**
  - a1 matches the PDF example and T11, and preserves shortcuts in viewports. a2 also preserves keys but marks every viewport unknown. Under a1 and a2, keys typed into custom-drawn text fields without UIA patterns (for example Blender's own fields) are published as keys; M2 adapter text-field events close that gap. a3 loses all keys in pattern-less viewports, which is the review's main C-1 effect.
  - b2 publishes key identities where the field may accept text and the editability read failed. INP-4 treats that case as redacted, so b1 is the conservative reading.
  - c1 publishes keys in a field that is partly editable, with typing marked unknown. It follows CTX-9 and ALG "conflicting evidence". c2 is more conservative but loses those keys.
  - d1 keeps redaction for a classification of the wrong element. d2 trusts the focused-element query when the provider does not implement `HasKeyboardFocus`.
  - Unchanged in every case: read failure or timeout → redacted; IsPassword unknown → redacted; password → redacted.
- **Recommendation: a1, b1, c1, and d1 combined with d2.** Stage C2 is blocked by this decision. Stage C1 contains only the corrections that do not change redaction in a disputed direction.

### D-5. Suspend when the open segment cannot finalize within 2 s
- **Sources.**
  - CLK-7: "On PBT_APMSUSPEND, finalize the open segment within 2 s; one not finalized stays .part for REL-3."
  - REL-3 is deferred (A2).
  - A2: "Normal operation remains required: … lock/sleep boundaries".
  - REL-9 has the same structure for session end.
  - The code fails the attempt.
- **Options.**
  - **A. Bounded wait, then continue.** The controller waits at most 2 s for the suspend finalization, then lets the system sleep. The video worker keeps finalizing (it resumes after wake) and publishes normally. A `suspend_finalize_late` health row records the miss. The attempt continues in Paused(Sleep).
  - **B. Abandon the open segment.** Leave the `.part` unpublished, account its slots as missing with reason `suspend_unfinalized`, and continue the attempt.
  - **C. Keep failing the attempt.**
- **Consequences.**
  - A loses nothing unless power is lost during sleep (then the `.part` remains, which REL-3 handles later).
  - B is simpler but loses up to 60 s of video at every slow suspend.
  - C turns ordinary sleep into attempt failure, which A2 forbids.
  - Stage V3 already removes the main cause, the synchronous decode, so the 2 s budget will usually be met.
- **Recommendation: A.** Stage V5 is blocked by this decision. Session end (REL-9) keeps its separate 2 s and 4 s budgets and the existing leave-`.part` rule.

### D-6. "Complete" when a selected monitor is absent
- **Sources.**
  - SPEC §3 and README: Complete means "usable video for each selected monitor".
  - A2 and VID-9: monitor removal is normal operation and "none ends the attempt".
  - REL-7: complete "only if the segment finalized and every flush succeeded".
  - Code: a monitor counts as unavailable only after an encoder failure. A selected monitor with zero encoded frames while another monitor has video gives `VideoUnavailable`, so a monitor that was unplugged for the whole attempt makes it Incomplete.
- **Options.**
  - **A. Interval rule.** Complete requires usable video for every selected monitor for each interval in which that monitor was present and admitted. Absence intervals are recorded (`monitor_missing`, missing slots) and do not prevent Complete.
  - **B. At-least-once rule** (close to the current behaviour). Every selected monitor must have encoded video at least once.
  - **C. Strict rule.** Any absence makes the attempt Incomplete.
- **Consequences.**
  - A matches "monitor removal is normal" and the PDF REL-7 criteria.
  - B fails a valid attempt in which an unused selected monitor was disconnected before Start completed.
  - C contradicts VID-9 and A2.
- **Recommendation: A.** Stage S3 is blocked by this decision.

### D-7. Dense motion and wheel association
- **Sources.**
  - PDF INP-6: "Movement sampling runs only after raw and hook records are matched … Keep all samples while a button is held".
  - ALG §6.1:
    - "Motion is not assumed to be one raw packet per hook. Preserve hook samples during held motion; group unambiguously associated raw deltas into a sample with a coalesced count. If coalescing/device ambiguity prevents association, preserve the raw evidence and hook evidence as unpaired observations".
    - "Match only an unambiguous order-consistent pair … do not choose merely the nearest timestamp".
  - No source defines what an unpaired raw observation looks like in the journal.
- **Options.**
  - **A. Hook-anchored association.**
    - Every approved hook motion is a sample.
    - All raw relative packets received between that sample's predecessor and itself, inside the same continuity interval, are attributed to it as `delta` + `coalesced` when exactly one relative raw device produced motion in that interval. Otherwise `delta` is null with reason `raw_association_ambiguous`.
    - Raw deltas that are never associated stay in the per-second raw-motion evidence (per device: packet count and summed delta) in `input_health`. They are not counted as cursor moves.
    - Wheel: inside a maximal run of same-axis wheel events from one raw device, with no gap or barrier, pair the k-th raw with the k-th hook when the run's raw and hook counts are equal. Otherwise the run's events are published unpaired, with hook values and null raw values.
  - **B. Publish unpaired raw packets as their own `raw_motion` rows** (a new kind), and pair hooks only when strict 1:1.
  - **C. Strict matching** (current behaviour, loses data).
- **Consequences.**
  - A preserves every hook sample (positions are what INP-5 and INP-6 need). It never copies one delta onto several samples, and its row volume equals the current design.
  - B preserves raw packet granularity, at up to 1000 rows/s per mouse and with a new kind.
  - C violates T7 and C3.
- **Recommendation: A.** Stage I2 is blocked by this decision.

### D-8. Elevated: target changes during preparation, and an ineligible approved foreground after admission
- **Sources.**
  - ELEVATED §4.1: "Revoked proof, missing required access, expiry or host failure blocks startup or resume"; "If necessary, wait within readiness for the exact candidate to return to the foreground."
  - §4.2 "Unapproved or unknown foreground …" row: the collector path "closes the associated grant and waits for a fresh valid foreground".
  - §4.2 last row: "required readiness failure … After Start, stop and finalize only the provable prefix as a failed attempt".
  - §1 (inherited rights): "An incompatible, cross-sign-in or uncertain foreground closes admission or is suppressed".
  - ALG §4 item 5: "Unknown or unapproved foreground stays suppressed; a confirmed required source fault takes the failure path."
  - Code: any foreground or focus change during Preparing/Armed fails the attempt. An approved foreground with an unsupported token fails the attempt, even in a standard epoch.
- **Options.**
  - **A. A target change is a transition, not a fault.**
    - A foreground or focus change during Preparing, Armed or Activating revokes the grant.
    - Before admission, the selector waits within the remaining readiness budget for the same candidate (startup consent stays bound to it). If the budget expires, startup fails without an admitted recording.
    - After admission, the epoch stays closed with `ElevationTransition`, and the actual new foreground is classified fresh.
    - An approved foreground with an unsupported token (DifferentUser, DifferentSession, Protected, failed linked pair) is suppressed like an unapproved foreground, with a typed reason.
    - Only host, transport, authentication, lease, required-access-on-the-current-grant and readiness-deadline failures fail the attempt.
  - **B. Keep fail-fast** for all of the above, and amend the documents.
- **Consequences.**
  - Under A, an Alt+Tab during UAC readiness or a focus move in the target no longer destroys the attempt.
  - Under B, the attempt depends on a race between the collector's `Invalidated` notice and the Recorder's heartbeat, and user behaviour that is normal on a standard source ends a protected attempt.
- **Recommendation: A.** Stage E5 is blocked by this decision.

### D-9. Collector-side approval proof
- **Sources.**
  - ELEVATED §4.1: "Collector authorization requires both core canonical matching and the collector's independent proof, with no approval by basename, title, HWND or PID alone"; "It never opens a supplied target path."
  - Code: the collector checks that the key belongs to the sealed list and that the claimed image spelling equals the live process image. Only the Recorder canonicalizes and matches.
- **Options.**
  - **A. Independent canonicalization in the collector.** The collector resolves the live process image itself: it opens the image file of the pinned live process handle, never a supplied path, and takes its final path by handle and its file ID. It then requires equality with the sealed approval key's canonical identity.
  - **B. Keep the current split** and clarify in the document that the collector's proof is the live spelling identity.
- **Consequences.**
  - A closes the case where a compromised or buggy Recorder sends a wrong key–spelling pair. It adds file-handle work on the collector's prepare path, not on any callback.
  - B needs only a document change.
- **Recommendation: A.** Stage E6 is blocked by this decision.

### D-10. When a video slot is decided, and which late frames it may take
- **Sources.**
  - PDF VID-4: "keep the newest WGC frame in each 1/fps slot … A slot with no new frame repeats the previous one … it is late when a changed frame … has its source_t_us inside it". Slot membership is defined by source time.
  - ALG §9.1: "Decide a slot when its interval closes. Select the newest eligible captured frame for that slot, or repeat the previous selected frame if none arrived." The ALG does not say whether "closes" refers to source time or receipt time.
  - Code (`video.rs:180-186,417`) decides the slot at `now ≥ end` and additionally requires `recv_t_us < end` and `available < end`. A frame whose source time is inside the slot but which arrives after `end` is discarded, and the slot repeats the older frame (V-2).
- **Options.**
  - **A. Source-time watermark with bounded grace (amends the ALG §9.1 decision point).**
    - Slot k is decided when a frame with source time ≥ `end_k` has been received, or when `now ≥ end_k + G`.
    - Any frame with source time in the slot that becomes available before the decision is eligible.
    - G = one capture pacing interval plus a conversion allowance capped at two slot intervals. G is recorded as an engineering setting.
  - **B. Keep receipt-time acceptance** (current behaviour). Document it as an amendment: a slot contains only frames received before its end.
- **Consequences.**
  - A changes which frame the video shows for a slot. On a static screen, a single change appears in its own slot instead of one slot later. Slot decisions are delayed by at most G, which adds at most two frames of buffering inside the existing bounds. `late` keeps its VID-4 meaning.
  - B keeps the current timing and the V-2 effect: roughly one late slot for single changes, flagged `late:true`.
- **Recommendation: A.** Stage V2b is blocked by this decision. The rest of V2 (V2a) is not.

## 6. Cross-cutting design

### 6.1 Terminal outcomes and the loss ledger (cause C-A)

**Rule.** Every item an owner accepts reaches exactly one terminal outcome.

| Outcome | Meaning | Persisted evidence |
| --- | --- | --- |
| Persisted | Row or file written | The row itself |
| Suppressed | Withheld by a policy decision: approval, privacy, control chord, pause | Counted only. Sensitive suppressions go into the per-visit redacted aggregate; others into the suppression counters of the stream summary and, per interval, into `input_health` |
| Dropped | Lost because of capacity, malformation, deadline or source loss | Ledger entry, emitted as a typed `gap` row in the same stream |
| Unavailable | A required observation could not be made (no frame, provider timeout, unknown device) | An explicit row with `outcome`/`reason` where the stream defines one (still, element, focus); otherwise a ledger entry |

Correlation quality is never a terminal outcome. It is metadata on a Persisted row.

**Ledger data model.** The ledger is owned by each stream's `JournalIngress`. It replaces the single `Loss`.
- **Key:** `(reason, producer)`.
  - `reason` comes from a closed typed enum, for example: `queue_overflow`, `native_queue_overflow`, `transport_overflow`, `malformed_packet`, `unknown_device`, `resync_pending`, `foreground_inaccessible`, `producer_stall`, `matcher_capacity`, `commit_capacity`, `commit_too_long`, `commit_invalid_utf16`, `late_commit`, `clipboard_queue`, `cursor_asset_queue`, `notification_overflow`, `staging_pressure`.
  - `producer` is an optional typed producer identity: Raw Input, keyboard hook, pointer hook, HID-optional, a context source, a collector producer.
  - The key space is finite, so the ledger is a bounded array or map with at most one pending entry per key.
- **Value fields:**
  - `count`: Known(n) or Unknown. An Unknown count stays Unknown when merged with Known counts.
  - `first_t_us`, `last_t_us`: original session receipt bounds of the missing items.
  - Optional `first_source_seq`, `last_source_seq`: native producer sequence range.
  - Optional `first_qpc`, `last_qpc`: for collector evidence.
  - `admitted`: the receipt at which the gap row is published.
- **Merge rule:**
  - On the same key: sum the counts (saturating), take the min of the firsts and the max of the lasts, and keep the sequence ranges as min/max.
  - Different keys never merge.

**Emission rules.**
1. A pending entry is emitted as a `gap` row before the first later row of the same stream is appended (ALG §6.4). Multiple pending entries are emitted in ascending `first_t_us`.
2. **Receipt time.** The row's `recv_t_us` is its admission receipt. That is the maximum of the triggering receipt and the stream's last appended receipt, so journal order is preserved. The range fields keep the original loss times.
3. **Boundaries.** At a pause boundary, all pending entries are emitted before the pause row, never inside the pause interval (gate 4). At a resume boundary, losses noticed during the pause are emitted after the resume row, with their original ranges. At close, pending entries are emitted before the stream's final health rows.
4. **Reserved capacity.** The ledger lives outside the data queue. Gap and health rows use the reserved boundary capacity, so a full data queue cannot swallow them (architecture addendum §5). Failure to append a gap row is a journal failure (§6.2 class F). It never produces a further health row about itself (ALG §10.1, no recursion).
5. **Privacy.**
   - The ledger key includes a privacy class, `Ordinary` or `Sensitive`.
   - A loss is `Sensitive` when its items belong to a password visit, a possibly sensitive visit, or an approval-unknown receipt.
   - `Sensitive` entries record a count only. They have no first or last times, no sequence ranges and no QPC ranges, and they are never emitted as gap rows.
   - A key record lost before its visit is classified is `Sensitive`, since its visit may be sensitive. Pointer and HID records lost the same way stay `Ordinary` with their ranges.
   - The count is added to the open redacted aggregate of that visit. Approval-unknown losses go only to the stream summary's suppression totals.
   - This follows ALG §7.2 ("Aggregated health may count suppressed work without identifying keys") and PRIV-5 ("never per-key timing").
6. **Frames stream exception.** The frames ingress never records a loss. Frame rows are submitted with blocking backpressure only. Any video loss is a slot row with a reason, not a journal gap.
   - This keeps P-1's rejection valid. `metadata.rs:563` (`dropped != 0` → inventory failure) remains an assertion of this invariant.
   - The §6.5 merge and the V1 capacity must preserve the invariant, through per-monitor lanes, worker-side staging and independent progress marks.
   - The video worker never blocks on another monitor. The only non-row outcomes are:
     - an overdue verification, resolved by the worker as `segment_unverified` rows (L-required, §6.2 row 19);
     - a staging invariant breach (F, row 20);
     - a worker blocked in a native call (F, row 21).
   - Rows can be missing only under row 21, and then only in an attempt that has already failed.

**Counters.** `StreamSummary` gains per-stream totals of offered, persisted, suppressed (by policy class), dropped (by reason) and unavailable (by reason) (architecture addendum §5). The reader reconciles the gap-row sums against `dropped`.

**Runtime health rows.** Health rows are written when the condition occurs, not only at Stop.
- Placement: `input_health` goes in the input stream and capture health (CTX-7) in the context stream. Under D-1 option B, the current stream placement is kept.
- Each row records: kind, component, cause code, detection receipt, the first affected time where known, and whether the condition is ongoing.
- During a pause, health conditions are counted, not written. The counts go into the resume row (PRIV-2), or at Stop into the final health rows.
- A health row never has a null `recv_t_us`. A failure before the clock starts goes only to diagnostics and the manifest.

**Manifest health summary.** The manifest gets a `health` block with:
- `pauses`, `gaps` (sum of gap rows), `blind_intervals`, `dropped_stills`, `recovered_prefix: false`;
- per-stream `dropped_by_reason` and `suppressed_by_class`;
- `activity_losses`, a list of component, first time and cause (§6.2).

### 6.2 Fault classification (cause C-B)

The controller owns this classification. Every owner reports a failure as a typed value, `FaultReport{component, class, cause, first_affected_t_us, detected_t_us}`, over the existing fact path (stage A2) or the value channel (stage S4). No owner decides the attempt outcome itself.

There are three classes, with L split into two kinds:
- **F: attempt failure.** The integrity of already published output cannot be maintained, or a contract names the condition as attempt failure. Effect: the Stop path runs with the first cause retained, and diagnostics are written.
- **L: activity loss.** One activity can no longer produce data for the rest of the attempt, and no contract defines that as normal. Effect: that activity stops. A runtime health row and blind or unavailable intervals are recorded. The UI immediately shows "Recording degraded: <component>". The attempt continues until the user stops it. Other activities are unaffected. L is split into two kinds:
  - **L-required**: input, or the video of one monitor.
  - **L-auxiliary**: stills/PNG, UIA, clipboard, cursor assets, identity resolution.
- **G: recorded loss.** A bounded, local or transient condition. Effect: a ledger entry, an Unavailable row, or a time-limited blind interval.

**Complete list of conditions and escalation boundaries.** No condition outside this table changes class.

| # | Condition | Class | Boundary or source |
| --- | --- | --- | --- |
| 1 | Journal append, flush or close failure of any stream | F | Integrity; current behaviour |
| 2 | Manifest publication failure; required finalization failure at Stop (unverified or unfinalized required segment at the Stop deadline, including a segment interrupted during IDR rotation that is unpublished when the Stop deadline has expired) | F | REL-7, C6 |
| 3 | Clock failure; controller failure | F | Current behaviour |
| 4 | Capture-thread failure that is not a topology change (for example, GPU device loss) | F | Current behaviour retained. A2 defers device rebuild, and no finding requires a change |
| 5 | Admitted collector source failure (ELC-5): host or transport failure, authentication or protocol failure, lease loss, witness regression, readiness deadline expiry, required-access failure on the current grant, a failed standard-helper check when switching back from the collector (`StandardHelperUnavailable`) | F | ELC-5, ELEVATED §4.2, ALG §4 item 5. Conditions that are transitions rather than faults are governed by D-8 |
| 6 | Collector provider call still running at request deadline + 750 ms. The supervisor terminates the collector | F | ELEVATED §3.3 ("forcibly terminate"), §5.2 ("terminated by the supervisor") with ELC-5 |
| 7 | Collector input-lane write outstanding beyond the 500 ms essential coverage stall, or control-lane write beyond the lease | F | ALG §6.4 ("a persistent required-path coverage stall is a source failure") |
| 8 | Collector request deadline expiry where the call returns before deadline + 750 ms | G | ELEVATED §5.2 typed unavailability |
| 9 | Encoder failure or rejected segment verification on a monitor; a segment close or rotation part that fails before the Stop deadline expired; an encoder release failure at a format change or at Stop | L-required (that monitor's video) | VID-6, A2; current behaviour |
| 10 | Helper process exit or transport break | L-required (input) | A2 (no restart); current behaviour (`input_failed`) |
| 11 | Input producer progress stall: blind interval from 250 ms, escalating at 5 s | G below 5 s; L-required (input) from 5 s | 250 ms: current stall bound; 5 s: REL-5 heartbeat interval |
| 12 | Required Raw Input registration loss, or required hook loss (HookHealth: 20 unmatched transitions) | D-2 B: L-required (input), interval open until Stop. D-2 A: G per repaired episode, L-required (input) on a second failure within 5 s | INP-1, INP-12, D-2 |
| 13 | Opening or resume resync not possible; non-ordinary foreground | G (time-limited blind interval) | INP-13 |
| 14 | UIA client open failure, or one UIA call blocked longer than two transaction timeouts (1 s) | L-auxiliary (UIA) | ALG §7.1 ("diagnose the worker … no replacement") |
| 15 | Identity resolver blocked in one native call longer than the 2 s resolution bound | L-auxiliary (identity resolution) | README resolver contract |
| 16 | PNG worker exit | L-auxiliary (stills) | A2 |
| 17 | Clipboard worker exit | L-auxiliary (clipboard) | CTX-8 isolation |
| 18 | Queue overflow; malformed packet; unknown device; optional device failure; display enumeration race; identity resolution failure for one spelling; hotkey stall; suspend finalize late (D-5 A); per-request UIA, IME or element timeout | G | REL-2, VID-9, ALG §7.1 |
| 19 | A segment's verification result (including the wait for a verifier slot) is not received within the V3 verification allowance. The actor is that monitor's video worker, which resolves its own rows | L-required (that monitor's video) | §6.5 "verification overdue" |
| 20 | A video worker's frames staging still overflows after a one-slot-interval wait for lane space. This is an invariant breach of §6.5 | F | §6.1 rule 6: the frames stream cannot lose rows |
| 21 | A video worker's frames mark does not advance for T (segment duration + rotation allowance + verification allowance + one slot) while the controller is Recording and its monitor is Open with a current-epoch mark: the worker is blocked in a native call. T is suspend-excluded, measured through the S0 accessor, and is not evaluated during an unresumed suspend. A monitor Closing after a pause boundary (including Paused(Sleep)) is out of scope. The actor is the frames journal owner (only with the §6.5 merge, D-1 A) | F | §6.5 "worker blocked"; a live worker always produces real-time rows; A2 (sleep is normal operation) |

**Rules.**
1. **Escalation.** A condition escalates only at a boundary listed in the table (rows 11, 12, 14, 15, 19, 20 and 21).
2. **Primary cause.** The first F or L cause is retained as the primary cause, and later causes are kept as secondary. A Stop-deadline timeout never replaces a recorded cause (review §3, session item 1).
3. **Component.** `FailureComponent` comes from the reporting owner (capture, video, encoder, input helper, receiver, context, UIA, stills, PNG, journal, metadata, controller, collector, UI), never from a fixed default.
4. **Disposition mapping.** The manifest keeps its two fields: `state` ∈ {`complete`, `incomplete`} and the capture disposition. The first matching line applies.

   | Condition | `state` | Disposition |
   | --- | --- | --- |
   | Any F | `incomplete` | `failed` |
   | Any L-required on video, or a violation of the D-6 monitor rule | `incomplete` | `video_unavailable` |
   | Any L-required on input, or an input-certification shortfall at Stop (§6.3) | `incomplete` | `input_unavailable` |
   | Otherwise | `complete` (if video was recorded) | `recorded` |

   The existing `approval_suppressed` and `stopped_before_first_frame` dispositions keep their current rules. L-auxiliary and G conditions never change `state` or disposition; they appear only in `health`. This is SPEC §3 ("Complete denotes verified required closure and usable video for each selected monitor") and C6 ("may still contain explicitly recorded losses").
5. **No outcome change for current behaviour.** Encoder failure and helper exit already produce `video_unavailable` and `input_unavailable` today (`metadata.rs:705-718`). This matches A2 ("Stop the affected activity and report the recording as failed/incomplete") and ALG §11 ("Helper/encoder fails: error and report, truthful incomplete/degraded data status; no automatic recovery"). REL-8 (helper restart) is deferred by A2. Mapping helper exit to L-required (input) therefore changes no outcome and needs no decision (question Q1 of the critic review).

### 6.3 Closing and certification of the input prefix (cause C-C)

The matcher currently certifies an item only when the global floor exceeds `t + 40/60 ms`. At closing, this horizon cannot be reached, because the producers have stopped at the cutoff. The new rule:
- An item with receipt `t < cutoff` is **decidable** when either:
  - every producer's progress is at or beyond `min(t + window, cutoff)`; or
  - the producer has closed, meaning its final progress was received.
- At a closing boundary, every item below the cutoff is decided with the candidates actually present. Missing partners are treated as at expiry (ALG §6.1 item 4).
- `Discontinuous` is reserved for items whose candidate window crosses a recorded gap, barrier or device removal. It is metadata, not a reason to suppress.
- After a producer's final progress, no further partner can arrive, so the decision is exact.
- **Boundary waits for final progress.**
  - At Pause, the existing 250 ms wait is kept.
  - At Stop, the wait extends to the Stop drain deadline minus the reserve needed for finalization and manifest publication. This reduces spurious tails without a new deadline.
- **Undecidable items.** A producer that has not delivered final progress within the applicable wait leaves the items after its last progress undecidable. Those items are resolved as Dropped with reason `producer_stall` over `[last progress, cutoff)`. This is a Sensitive-class entry if the visit was sensitive.

**Effect on disposition.** This is the reconciliation with §6.2.
- **At Pause.** The undecidable range becomes a blind interval (`producer_stall`) before the pause row. It is a G condition (row 13 or 11 of §6.2) and does not affect the disposition. The prefix up to the pause is certified together with that explicit blind interval.
- **At Stop.** The undecidable range is recorded as `uncertified_closing_tail`. C6 makes "the admitted prefix is certified" a precondition for Complete publication, so a non-empty tail at Stop is an **input-certification shortfall**: disposition `input_unavailable`, state `incomplete` (§6.2 rule 4). This keeps the current outcome (`metadata.rs:518,710`); it is not a G condition.
- **Behaviour change.** The only change is that ordinary Stops no longer produce a tail: the 40/60 ms horizon is removed and the Stop wait is longer. Whether ordinary Stops produce a tail today cannot be determined statically (§9).

### 6.4 Foreground attribution for hook-only records (cause C-C)

ALG §6.2 requires a receipt-time foreground "from available ordered context evidence" for hook-only records. The Receiver keeps a bounded **foreground timeline** fed by the Context owner's ordered foreground transitions. Each entry holds:
- the approval identity, or none;
- the access classification;
- the conservative change interval `[tick − 16 ms, receipt]` (CLK-2 uncertainty).

A hook-only record with receipt `r` is approved when all of the following hold:
1. `r` lies strictly inside a stable segment of the timeline, with no change interval overlapping `[r − 16 ms, r]`.
2. That segment's foreground is approved and has ordinary access.
3. The timeline's progress covers `r`, meaning the Context owner has processed WinEvents up to at least `r`.

If condition 3 is not yet met, the record waits within the existing classification wait (ALG §3.2 progress rule). If it is never met, or conditions 1–2 fail, the record is Suppressed with class `approval_unknown`, counted as Sensitive (§6.1).

**Additional conditions for keyboard records** (keys and VK_PACKET), which carry text or key identity:

4. **Focus.** The timeline also carries the visit and focus-generation transitions with the same uncertainty interval. One visit and one focus generation must cover `[r − 16 ms, r]` without a change.
5. **Classification.** That focus generation's classification must be resolved before the gate's existing 500 ms classification deadline, measured from the first unresolved key. Publication then follows the classification exactly as for raw-proven keys:
   - non-text → keys preserved;
   - editable → text rules;
   - password, possibly sensitive, unresolved, or a deadline expiry → the record is counted into the visit's redacted aggregate. It is never published per key.
6. **Uncertain focus.** If condition 4 fails, meaning a focus change is inside the uncertainty interval, the record is treated as possibly sensitive and is not published per key. If either adjacent visit has an open redacted aggregate, the record is counted into it. Otherwise it is counted as a Sensitive `focus_uncertain` suppression: a count only, with no times (§6.1).

Pointer records require only conditions 1–3. Since the per-process approval decision of 2026-11-18 (SPEC addendum A7.8), a pointer or other non-key record that carries Raw Input evidence, raw only or raw plus hook, needs only the approved process instance in the foreground at its raw receipt, whatever its focus. A key with a hook part still needs a known focus. Hook-only records keep the window-level visit conditions above.

Timeline capacity: one entry per transition, pruned below the Receiver's published prefix. It is bounded by the existing context notification cap.

### 6.5 Frames stream ordering with capture receipts (D-1 option A only)

With `recv_t_us` set to the capture receipt of the selected frame, the frames journal must merge monitors locally (ALG §3.2). The receipt rule holds under both D-10 options: D-10 changes only when d_k is taken and which frames are eligible. The frames serializer still uses d_k, so stage R1 depends on D-10. With D-10 = A (chosen), V2b must be implemented before R1 fixes the frames serializer.

**Real-time slot rows.** Every selected monitor's video worker decides every non-paused slot in real time and produces its row at decision time. This includes slots while the monitor is absent (`MonitorMissing`), while capture is unavailable, and after the monitor's own video L fault (`EncoderUnavailable`) (VID-4: "lines also mark slots … while the monitor is unavailable"). A worker therefore never stops producing rows before Stop. Monitor removal and return are capture-epoch changes inside the same admission epoch, and they do not change the set of merge producers.

**Receipt rule (amended, accepted by the user 2026-11-17; replaces the earlier `max(prev, min(d_k, L_k))` rule).** Rows are defined per monitor, in slot order across all epochs of the attempt. For row k:
- d_k is the slot's decision receipt, kept as `slot_decided_t_us`.
- e_k is the end of slot k, limited by the epoch's cutoff when the epoch closes inside the slot.
- **Row that newly selects a frame f:** `recv_t_us` = receipt(f), the true capture receipt. If receipt(f) is below the previous row's `recv_t_us`, f is not selected: it is counted in `frames_rejected` and the slot repeats (this replaces the former fatal `FRAMES_ORDER`).
- **Every other row** (a repeat, or a row without a frame): `recv_t_us` = max(`recv_t_us` of row k−1, e_k). This is an **ordering receipt**. It advances on a static screen and during absence, so the progress mark never freezes. The first row of the attempt uses e_k.
- A repeat keeps the PDF's `obs_id` and `source_t_us` of the repeated frame, but its `recv_t_us` is an ordering receipt (D1 documents this).
- A frame's receipt is max(dequeue, source), because the WGC source time is the DWM present time and can lead the dequeue by about one refresh (amended 2026-11-18, ALG §13.14). So receipt(g) ≥ source(g) holds for every delivered frame by construction. A source more than 42 ms after the dequeue is rejected (`rejected_time_or_epoch`).

**Monotonicity, within one monitor.** Every row's `recv_t_us` is either a max that includes the previous row's value or a frame receipt that is at least the previous row's value (anything else is rejected). So the sequence is non-decreasing by construction.

**Why rejection does not happen in normal operation.**
1. WGC delivers a monitor's frames in source-time order, and the capture loop dequeues them in processing order. Both terms of max(dequeue, source) are non-decreasing, so source(f) < source(g) implies receipt(f) ≤ receipt(g).
2. **Invariant I.** For every row i and every frame g delivered to the worker with source in a slot after i: `recv_t_us`(i) ≤ receipt(g).
   - If row i newly selects f, then f is in slot i and g is in a later slot, so receipt(f) ≤ receipt(g) by item 1.
   - Otherwise, `recv_t_us`(i) = max(`recv_t_us`(i−1), e_i). The first term is covered by induction. For the second term, e_i ≤ source(g) ≤ receipt(g).
3. So a frame is rejected only if WGC order or receipt stamping is violated. The counter makes such a violation visible without failing the attempt.

**Pause, resume, absence.**
- Every row of a closed epoch has `recv_t_us` ≤ its cutoff, which is < the next opening R.
- Slots after a resume start at or after R. So every frame in them has receipt ≥ R, and every e_k is > R. The first post-resume row is therefore ≥ R.
- Rows are produced throughout an absence (real-time slot rows). A returned monitor's first new frame has source after every earlier slot, so invariant I covers it.

Ties are broken by (`recv_t_us`, monitor, slot). D-10 A changes only d_k and which frames are eligible, not the rule.

**Merge without blocking or deadlock.**
- **Lanes.** Each monitor's video worker has its own bounded lane to the frames journal owner (single-producer, single-consumer), with capacity reserved per monitor. One monitor's full lane never consumes another monitor's capacity.
- **Worker-side staging.** The worker never blocks on its lane. Resolved rows that do not fit stay in the worker's own staging, which is bounded by V1 and also holds rows awaiting verification. Encoding continues.
- **Progress marks.** Each worker publishes an atomic mark p. All of the worker's rows with `recv_t_us` < p are already in its lane. Rows that are resolved but not yet enqueued, and unresolved rows, all have `recv_t_us` ≥ p. The mark is updated independently of the journal owner's state.
- **Mark computation.** Rows are non-decreasing in slot order. The mark is a key: p_X = (`recv_t_us`, X, slot) of the worker's first row (in slot order) not yet enqueued. If every decided row is enqueued, p_X = (`recv_t_us` of the last enqueued row, X, next slot). Every future row of X has a key ≥ p_X. Each mark is tagged with the admission epoch the worker is in.
- **Boundary ownership.** The boundary owners set the merge constraints; workers do not.
  - **Merge state.** The frames journal owner keeps a state per monitor, Open(epoch e, floor F) or Closed. The constraint c_X is:
    - in Open(e, F): p_X if the latest published mark of X carries epoch e, else F;
    - in Closed: +∞.
  - **Opening.** At Start and at every resume with receipt R, the controller sends Opening(e, R) to the frames journal owner and waits for its acknowledgement *before* publishing epoch e to any video worker.
    - The owner processes control messages before lane rows. On Opening it sets every selected monitor to Open(e, F = (R, X, 0)).
    - Rows with `recv_t_us` ≥ R can exist only after a worker has learned e, which is after the acknowledgement. Until each X publishes an e-tagged mark, c_X = F blocks every row ≥ R. This closes the resume race.
    - The merge-owner thread processes Opening and sends the acknowledgement. That thread only merges lanes and hands released rows to the writer. File appends and flushes run on the writer side, so a slow disk flush cannot delay the acknowledgement. A missing acknowledgement within the boundary deadline therefore means the merge owner itself has failed, which is a journal failure (F, §6.2 row 1).
  - **Closing.** At pause and at Stop, each worker enqueues the last row of the closed epoch and then publishes a closing mark for e. On that mark, the owner sets X to Closed.
    - Closed X does not constrain: X's next row has `recv_t_us` ≥ the next opening R′ (proof item 4), and no row ≥ R′ can exist before Opening(R′) reaches the owner.
    - After Stop, monitors stay Closed.
  - **Monitor absence and return.** These need no merge registration. Real-time slot rows keep X Open with an advancing mark for the whole admission epoch. The former "removed monitor stops constraining" rule is withdrawn.
- **Release rule.** The journal owner releases the smallest lane-head row r (by key) when r < c_X for every selected monitor X.
- **Segment flush.** When a segment's outcome is final, its worker asks the journal owner to flush the frames journal through the segment's last row. The flush waits until the merge has released that row; a segment with no rows requests the prefix written so far immediately. Requests still pending when the merge finishes are flushed then.
- **No deadlock.**
  - Let r_min be the smallest lane-head row, in monitor X's lane. Then r_min < p_X, so for Open X, r_min < c_X.
  - Every monitor Y with a non-empty lane has c_Y ≥ p_Y > head_Y ≥ r_min. If Y is still at floor F, its lane is empty.
  - A Y with an empty lane is either Closed (c_Y = +∞), or its worker is free to enqueue and advance p_Y. Advancing p_Y needs nothing from the journal owner.
  - So either r_min is releasable now, or every blocking constraint belongs to a worker that is free to advance it. No cycle exists.
- **Capacity.** Per monitor, the lane plus the worker staging hold at least twice the V1 staged-slot bound (two verification cycles) plus the rows of the watchdog bound T below. A monitor's rows can wait for another monitor's mark that long without overflow.
- **Actors when progress stops.** The worker owns its staging, so it is the only actor that resolves its own rows. The journal owner only observes marks.
  - **Verification overdue (§6.2 row 19, L-required).** The video worker waits for each segment's verification result with a deadline, the V3 verification allowance. This includes the V3 wait for a free verifier slot, which is bounded by the oldest pending segment's allowance.
    - On expiry, the worker itself resolves that segment's rows as `encoded:false` with reason `segment_unverified` (the `.part` stays) and reports L-required for that monitor's video.
    - It continues deciding later slots in real time as `EncoderUnavailable`.
    - A late verifier result is discarded.
    - No other thread touches the staging.
  - **Worker blocked (§6.2 row 21, F).** If a worker's mark does not advance for T = one segment duration + rotation allowance + verification allowance + one slot interval while X is Open and in watchdog scope, the worker itself is blocked in a native call (for example `Finalize` or an MF call), because a live worker always produces real-time rows.
    - **Suspend exclusion.** T is measured as suspend-excluded elapsed time through the S0 accessor: QPC elapsed since the last mark advance, minus its overlap with every `[suspend QPC, resume QPC]` interval. While a suspend mark exists without a newer resume mark, the watchdog is not evaluated.
    - **Watchdog scope.** The watchdog applies only while the controller is Recording and X has published a mark for the current epoch.
      - From a pause boundary (including Paused(Sleep) at `PBT_APMSUSPEND`) until X publishes its closing mark, X is **Closing**. Under D-5 A, X may finish finalization only after wake.
      - A Closing X still constrains the merge through its mark, but row 21 does not apply to it.
      - The closing path's own bounds govern instead: the D-5 suspend wait, the pause bound, and at Stop the Stop deadline and required finalization (§6.2 row 2).
      - Sleep and a late post-wake finalization therefore never trigger row 21 (A2).
    - The frames journal owner, which already reads the marks, detects this and reports F (component video, monitor X). It does not access X's staging.
    - On the failure Stop path, the owner closes X at its last mark and releases the other monitors' rows.
    - X's undelivered slots, from its last mark to Stop, are recorded as a range in the failure diagnostics and the manifest health block.
    - This is the only case where frames rows can be missing, and it occurs only in an attempt that already failed (§6.1 rule 6).
  - **Staging overflow (§6.2 row 20, F).** If a worker's staging would still overflow (an invariant breach, because row 21 bounds every wait), the worker waits at most one slot interval for lane space and then reports F.
- **No loss.** In every attempt without an F, the merge never drops a row (§6.1 rule 6).

## 7. Implementation stages

**Execution mode (user directive, 2026-11-17). This mode overrides the per-stage verification wording below:**
- **No per-stage compilation.** One `cargo check --workspace --all-targets` runs after all stages are implemented, and its errors are fixed then. Completion criteria are checked statically during the work.
- **Testing is done by the user.**
  - Stage T2 (unit tests and fixture packages) and stage T4 (execution campaign) are removed. The test lists in T2 remain as guidance for the user's own testing.
  - Superseded in part on 2026-11-18 (SPEC addendum A7.6): a minimal test set is allowed, namely golden byte tests of journal rows, `validate_gates` on a synthetic attempt and `FramePolicy` unit tests. The user runs them.
  - Stage T1 is reduced to extending the existing `PackageReader::validate_v5_artifacts` (and its v6 successor in R1) with the gate 1–4 checks. No separate test framework or tool binary is added.
  - Stage T3 stays (the M1-08 trusted fixture Save producer), at its minimal form.
- **Minimal code.**
  - Implement each algorithm with the least new code that fully satisfies it: reuse existing types and paths, and add no new abstraction, layer, module or counter unless the stage requires it.
  - Remove code that a change makes obsolete.
  - Prefer extending an existing enum, struct or function over adding a parallel one.
- **Two developers in parallel on disjoint modules, then a single owner for R1 and D1:**
  - Track A (video, stills, session, package): V3, V4, V2b, V5, S1, S2, S3, S4, P1, P2, P3, P4, X1, T3, T1.
  - Track B (input, context, elevated): the I5 follow-up, I3, I4, I2, I6, I7, C3, C2, C4, C5, E1, E2, E3, E4, E5, E6.
  - Then R1 and D1, followed by the final compilation and fix-up.
- **Final minimalism pass.** Before the final compilation, review the whole diff against the baseline for redundancy (parallel types, unused fields or counters, duplicated logic, dead leftovers), starting with `video.rs`, and simplify.
- **Review in batches.** The critic reviews the batches each track hands over. Its remarks take priority over the next stage.

All stages are grouped by module and listed in dependency order. "Static" means a reviewer can confirm it by reading code. "Execution" means it needs the user's separate authorization to build and run. Every stage needs compilation before it can be considered finished, and compilation needs authorization.

| Stage | Title | Depends on | Blocked by |
| --- | --- | --- | --- |
| A1 | Loss ledger, runtime health rows and manifest health summary | — | — |
| A2 | Fault classification, first cause and component attribution | A1 | — |
| I1 | Correlation as metadata; closing certification; input gap rows | A1 | — |
| I2 | Dense motion and wheel association | I1 | D-7 |
| I3 | Hook-only attribution, injected origin and injector hints | I1 | — |
| I4 | Ordinary-key state across boundaries | I1 | — |
| I5 | Helper and Receiver liveness; non-fatal input faults; blind intervals | A1, A2, S0 | — |
| I6 | Native input hygiene (ARCH-2, INP-2/5/11/12 details) | I5 | — |
| I7 | Text commit status and commit loss accounting | A1, C1 (for the IME `unknown` state) | — |
| I8 | Registration and hook repair (only if D-2 = A) | I5 | D-2 |
| C1 | Focus read corrections that do not change disputed redaction | — | — |
| C2 | Classification of undetermined editability (D-4a–d) | C1 | D-4 |
| C3 | Context loss accounting, non-fatal identity, UIA worker diagnosis | A1, A2 | — |
| C4 | Context fidelity (Ghost, geometry, hint events, pointer reads, CTX-12) | C3 | — |
| C5 | Unapproved-foreground interval handling | A1, C3 | D-3 |
| V1 | FPS-derived video capacities | — | — |
| V2a | Repeat-source continuity and timeline-pending accounting | V1 | — |
| V2b | Slot decision watermark | V2a | D-10 |
| V3 | Segment verification off the video worker | V1, A2 | — |
| V4 | Non-fatal display and encoder selection; self-check placement | A2 | — |
| V5 | Suspend finalization | V3, A2, S0 | D-5 |
| S0 | Suspend and resume QPC marks; environment callback hygiene | — | — |
| S1 | Hotkey liveness and Stop availability | A1, A2, S0 | — |
| S2 | UI and publication outcome corrections | A2 | — |
| S3 | Completion rule for selected monitors | A2 | D-6 |
| S4 | Controller-owned facts and explicit state machine | A2, S0, S1, and V5 (or V3 if D-5 is open) | — |
| P1 | Still drop accounting and reasons | A1 | — |
| P2 | Retention fairness and trigger correctness | P1 | — |
| P3 | Shared physical image work per retained frame | P1 | — |
| P4 | Output inventory for images and cursors | P1 | — |
| E1 | Standard-source continuity during foreground classification | I5 | — |
| E2 | Collector provider deadlines | A2 | — |
| E3 | Collector loss continuity, lane pressure, drain and token checks | A1, A2 | — |
| E4 | Elevated closure and evidence completeness | A2 | — |
| E5 | Target change and ineligible foreground handling | E4 | D-8 |
| E6 | Collector canonical approval proof | — | D-9 |
| R1 | Journal schema conformance and versioning | A1, I1–I5, I7, C1, C3, P1, S0, V2a, and V2b if D-10 = A | D-1, D-10 |
| X1 | Dead code removal | — | — |
| T1 | Package gate checks (gates 1–4) | A1 (and R1 if D-1 = A) | — |
| T2 | Algorithm unit tests and fixture packages | T1 and the stages under test | — |
| T3 | Trusted fixture Save producer | — | — |
| T4 | Execution verification campaign | T1–T3 | user authorization |
| D1 | Documentation updates | all decisions; R1 | D-1…D-10 for the decision-specific parts |

Stages that are not blocked can run in parallel within these dependencies. A suggested order is: A1, A2, S0, V1, C1 → I1, I5, V2a, V3, V4, P1, C3, S1, S2, E2, E3, E4, X1, T3 → I3, I4, I6, I7, C4, P2, P3, P4, E1, T1 → S4, T2 → blocked stages as decisions arrive → R1 → D1 → T4.

---

### Stage A1. Loss ledger, runtime health rows and manifest health summary

**Context.** Only one fixed gap reason exists. Health and blind rows are written only at Stop. `StreamSummary` has no outcome classes, and the manifest has no `health` block. This stage provides the mechanism that most later stages report into (§6.1).

**Algorithmic changes.**
- **Ledger.** Replace the single pending `Loss` in `JournalIngress` with the keyed ledger of §6.1. Typed loss evidence carries `{reason, producer, count Known/Unknown, first/last receipt, optional source sequence and QPC ranges}`. `record_loss` and `record_loss_at` accept it.
- **Emission.**
  - Gap rows are offered before every data row, at every boundary (in the order of §6.1 rule 3) and at close.
  - The gap row carries `reason`, `producer`, `count` (or `null` with `count_known:false`), `first_recv_t_us`, `last_recv_t_us`, and optional `first_source_seq`/`last_source_seq` and `first_qpc`/`last_qpc`.
- **Runtime health.**
  - Add a runtime health submission, `submit_health(kind, component, cause, detected receipt, first affected, ongoing)`, on the input and context ingresses. It uses reserved boundary capacity.
  - While the controller reports a pause, health is counted in a bounded per-kind counter and emitted with the resume row or the final health rows.
  - The final health path no longer emits rows with a null receipt. Pre-clock failures go only to diagnostics.
- **Counters.** Every ingress counts offered, persisted, suppressed by class, dropped by reason, and unavailable by reason. `StreamSummary` carries them, with `serde(default)` for old fields.
- **Manifest.** Add a `health` block built from the stream summaries, pause and blind records, stills drop totals, and activity losses (activity losses are filled by A2). `recovered_prefix` is always false in M1.
- **Reader.** Accept the new gap fields. Reconcile `sum(gap.count where known)` with `dropped` (an unknown count is excluded from the sum and must be flagged). Check that the health block equals the stream totals.

**Affected modules and interfaces.**
- `core/src/package/journal.rs`: `JournalIngress` loss API, `StreamSummary`, final health.
- `core/src/package/metadata.rs`: manifest `health`.
- `core/src/package/reader.rs`.
- Callers of `record_loss_at` (input gate late text, still rows) are moved to the typed loss evidence.

**Finished result.** Any owner holding an ingress can report a typed, ranged loss and a runtime health condition. Both reach the journal in order without blocking. The manifest summarises them.

**Completion criteria (static).**
- The ingress holds at most one pending entry per key, outside the data queue.
- Sensitive entries carry no time, sequence or QPC range and are never emitted as gap rows.
- The frames ingress has no call site that records a loss. The frames stream is written by blocking submission only (§6.1 rule 6), and the `dropped != 0` inventory assertion stays in force.
- No code path constructs a gap row with a fixed reason string.
- Every gap row contains reason, count (or explicit unknown) and first/last receipts.
- Health submission uses reserved capacity, and the pause path counts instead of writing.
- No health row can carry a null `recv_t_us`.
- The reader reconciles the new totals.

**Verification.** Static: the above. Execution: the unit tests in stage T2 (emission order, boundary placement, merge).

---

### Stage A2. Fault classification, first cause and component attribution

**Context.** Conditions are fatal or silent without a consistent rule. The first cause is lost on a Stop timeout. `CaptureError::backend` always names `WindowsCapture`.

**Algorithmic changes.**
- **Fault reports.** Introduce `FaultReport` and the classes of §6.2 (F, L-required, L-auxiliary, G). The controller records:
  - the first F/L as the primary cause, the rest as secondary (bounded list);
  - L reports as `activity_losses` with component and first time;
  - the UI status flag `degraded`.
- **Existing paths.** Map the existing paths onto `FaultReport` as the §6.2 table lists, without changing the current outcome:
  - `video_failed` (encoder failure) → L-required video;
  - `input_failed` (helper exit) → L-required input;
  - `set_source_failure` → F;
  - journal `on_failure` and required finalization failure → F;
  - capture-thread failure → F.

  Stages I5, C3, V4, E2 and E3 then reclassify individual conditions, and only at the table's boundaries.
- **Frames inventory assertion.** The `dropped != 0` check for frames in `metadata.rs:563` stays. It asserts the §6.1 rule 6 invariant and is not a data-quality rule.
- **Stop deadline.** When the deadline expires before authorization, the result message and diagnostics use the retained primary cause if one exists (`session/windows.rs:1124-1127`). Only otherwise is the generic timeout used.
- **Failure component.** `CaptureError` carries the component of the failing owner. Remove the fixed-default constructor, and give each probe or start error its owner's component.
- **Disposition.** Implement the mapping of §6.2 rule 4 in metadata finalization. It has the same precedence as the current code (failed > video_unavailable > input_unavailable > recorded). L-auxiliary and G conditions are excluded from it explicitly. D-6 is handled separately in S3, which keeps the current monitor rule until then.

**Affected modules.** `core/src/session/windows.rs` (facts, run result), `core/src/session.rs` (status: `degraded`, primary cause), `core/src/error.rs`, `core/src/platform/windows.rs` (disposition and report composition), `core/src/package/metadata.rs`, `ui/src/app.rs` (degraded status line).

**Finished result.** One place decides the attempt outcome from typed reports, the first cause survives every deadline path, and the UI shows degraded recording while it continues.

**Completion criteria (static).**
- Every path that previously set a failure fact goes through `FaultReport` with an explicit class.
- `CaptureError::backend` with an implicit component no longer exists.
- The run result on a Stop timeout prefers the recorded cause.
- The manifest `health.activity_losses` is populated from L reports.

**Verification.** Static: the above. Execution: T4 scenarios (encoder failure, helper kill).

---

### Stage I1. Correlation as metadata; closing certification; input gap rows

**Context.** I-1 (keys, buttons, wheel part), I-2, I-4, the input loss items of review §2.1, and the unread counters.

**Algorithmic changes.**
- **Publication.** `approved_receipt` no longer rejects Ambiguous, MissingScan or Discontinuous. Publication follows approval and privacy only.
  - A record with raw evidence uses the raw `foreground_at_receipt` proof. If raw slots disagree on the proof, the record is approval-unknown and Suppressed (counted).
  - Hook-only approval moves to stage I3.
  - The row carries `correlation` and, for non-Matched outcomes, null hook-only or raw-only fields with the reason (ALG §6.1 item 4).
  - Pairing rules are unchanged: they still never choose the nearest timestamp.
- **Row formation for key, button and wheel clusters.** Motion is covered by I2 and D-7.
  - **Hook record states.** Every record in the matcher has exactly one terminal state:
    - `Paired`: in one row with its partner;
    - `RawAnchored`: the raw side of a cluster row;
    - `ClusterAbsorbed`: a hook consumed by a cluster, which yields no row of its own;
    - `HookOnly`: eligible only for the I3 path;
    - `Dropped` or `Suppressed`: through ledger accounting.

    A hook record becomes `HookOnly` only when its candidate window has closed with zero compatible raw candidates. A record that is `ClusterAbsorbed` or `Paired` can never enter the I3 path. This prevents double publication.
  - **Cluster.** A cluster is a connected component of the compatibility graph. Nodes are raw and hook records. An edge joins a raw and a hook record that are compatible under the existing rules (same class, identity and transition direction), within the ±20 ms window, in the same admission epoch, and not separated by a gap, barrier or device removal.
    - A component is decided when the candidate windows of all its members have closed under the certification rule (§6.3).
    - The current propagation of ambiguity to records outside the component, through order crossings (`matching.rs:387-392`), is replaced: a crossing marks only its own component Ambiguous.
  - **Outcome per component with N raw and M hook records.**
    - N = M = 1 with mutual uniqueness and no crossing → one `Paired` row. Correlation is `matched`, or `missing_scan` when either side lacks a scan code.
    - N = 0 → every hook is `HookOnly` (I3).
    - M = 0 → every raw becomes its own row with `correlation:"unmatched"`, as now.
    - N ≥ 1 and M ≥ 1, otherwise (ambiguous) → exactly **N rows**, one per raw record, in raw receipt order, with `correlation:"ambiguous"`. All M hooks are `ClusterAbsorbed`.
      - Raw fields (`vk`, `scan`, raw witness, device, raw wheel value, raw button) come from the row's own raw record.
      - Hook-only fields (`keyboard_hook` witness, event `pos`, hook wheel value) are null with reason `ambiguous`.
      - `origin` follows the INP-7 rule for an unmatched raw record (`unknown`, unless hook-loss handle rules apply). One exception: if every hook record in the component carries an injection flag (`LLKHF_INJECTED` or `LLMHF_INJECTED`) and every raw record has a NULL device handle, all rows of the component get `origin:"injected"` (INP-7 precedence, as in I3). Promotion signatures still take precedence. Under I3's pairing rule, injected hooks can join components only with NULL-handle raw records, so this case is exhaustive.
      - Each row carries `cluster_raw` = N and `cluster_hooks` = M.
      - If M > N, the M − N excess hooks are counted as Suppressed with class `cluster_excess_hook`. If the cluster's visit is sensitive, they are counted Sensitive.
  - **Bound.** A component is limited by the matcher's existing capacity (1024 pending). Overflow is a typed loss (A1), so no separate component cap is needed.
  - **Invariant.** Every raw record and every hook record ends in exactly one published row, or in exactly one ledger or suppression count.
- **Held updates.**
  - A published non-Matched `key_up` or button up updates the published held state normally. This removes the false-repeat path.
  - An Ambiguous record whose device is unknown keeps the existing `uncertain_edge` handling in private state.
- **Closing certification.** Replace the horizon test at closing with §6.3. `finish(cutoff)` decides every item below the cutoff whose producers have delivered final progress or progress ≥ `min(t+window, cutoff)`. Undecidable items are resolved as Dropped with reason `producer_stall`, and their range sets `uncertified_closing_tail`.
- **Gap rows.** `InputGap` (with native first/last sequence and QPC, plus `admitted_count`) is reported to the input ingress as typed loss evidence. The ranges are:
  - helper ring loss: native counts and ranges;
  - matcher capacity: count 1 per item, receipt range;
  - Receiver/Gate overflow: count and range;
  - opening-resync drops (`resync_pending`): counted with a range, never with payload;
  - drops for a non-ordinary foreground (`foreground_inaccessible`): counted, with a blind interval (stage I5 adds the interval).

  A key record lost before its visit is classified (`resync_pending`, `foreground_inaccessible`, `producer_stall`, `unknown_device`, `uncertified_source`, Gate overflow, `staging_pressure`) is counted as `Sensitive` (§6.1 rule 5): count only, no range. Pointer and HID records keep their ranges.

  `input_continuity_gap` is replaced by the typed gap row, emitted at the gap's admission receipt with the original range.
- **Counters.** `ProvisionalRequests.{suppressed, dropped, synthetic_suppressed, private_gaps}` feed the ingress counters (A1) and the per-second `input_health`, or are deleted where they duplicate them.
- **Uncertain control.** `ControlDisposition::Uncertain` records are Suppressed with class `control_uncertain` and counted. A recorder hotkey is never published.
- **Held release.** An uncertain modifier still held at a boundary is closed through the existing synthetic release when it was published.

**Affected modules.** `core/src/capture/input/matching.rs` (`take`, `finish`, correlation results), `core/src/platform/windows/input/gate.rs` (approval, rows, gap rows), `receiver.rs` (gap routing, counters, opening and foreground drops), `core/src/capture/input/control.rs` (no behaviour change; accounting only).

**Finished result.** Key, button and wheel transitions are never lost because of correlation quality. Every input loss is a ranged, typed gap row.

**Completion criteria (static).**
- `approved_receipt` contains no correlation-based rejection.
- `finish` at a closing boundary has no `+40/60 ms` horizon dependency.
- Every matcher record reaches one terminal state.
- `HookOnly` is assigned only on window closure with zero raw candidates.
- An ambiguous component produces exactly N rows.
- Ambiguity no longer propagates outside the component.
- Every early-return discard in `Receiver::accept` and the gate records a terminal outcome.
- No write-only counter remains.

**Verification.** Static: the above. Execution: T2 matcher tests (an ambiguous key pair publishes once with `correlation:"ambiguous"`; closing decides items up to the cutoff), T4 T7 drag scenario.

---

### Stage I2. Dense motion and wheel association (blocked by D-7)

**Context.** I-1, motion and wheel part.

**Algorithm (option A of D-7).**
- **State per continuity interval and pointer.**
  - The last emitted hook sample time `h_prev`.
  - A raw accumulator: per raw relative device, the summed delta, packet count, and first and last receipt since `h_prev`.
  - The continuity interval ends at any gap, barrier, device removal or boundary.
- **On a hook motion sample at `h`.** The sample is published when approved; sampling (ALG §6.2) still applies. Then:
  - If exactly one device's accumulator is non-empty for raw packets with receipt in `(h_prev − window, h + window]`, those not yet attributed, attach `delta` = summed delta, `coalesced` = packet count, `correlation:"matched"`, and clear the accumulator.
  - If several devices are non-empty, attach `delta:null`, `coalesced:null`, `correlation:"ambiguous"` and reason `raw_association_ambiguous`. Move each accumulator into the per-second raw-motion evidence.
  - If none is non-empty, `delta:null` with reason `raw_unavailable`.
  - Absolute packets are attached only as `raw_absolute` when exactly one such packet falls in the interval. They are never summed.
- **Raw packets never followed by a hook sample within the window** (hook missing, or raw-only throttling) go into the per-second raw-motion evidence. They are not counted as cursor moves.
- **While the pointer hook is missing or lost** (INP-6, INP-12), raw packets drive sampling, as the current hook-loss path does.
- **Wheel.**
  - A maximal run is a sequence of same-axis wheel records from both producers, with no gap or barrier, in which consecutive records are within the match window.
  - When the run's raw records all come from one device and the raw and hook counts are equal, pair them by ordinal.
  - Otherwise publish the run's hook records with `raw_wheel:null` and reason `wheel_association_ambiguous`. Raw-only wheel records are published unpaired, as now.
- **Bounds.** The accumulator holds one per device, bounded by the device inventory. A run is bounded by the existing matcher capacity. Overflow is a typed loss (A1).
- **Removed rule.** The `motion()` rule "each raw candidate has exactly one hook" is removed.

**Affected modules.** `matching.rs` (motion and wheel paths), `sampling.rs` (raw-only evidence), `gate.rs` (pointer row fields).

**Completion criteria (static).**
- No motion path returns Ambiguous for "more than one hook in window".
- Every raw relative packet ends attributed to exactly one sample or in raw-motion evidence.
- No delta is attached to two samples.

**Verification.** Execution: T2 synthetic 1000 Hz streams (one device, two devices, hook loss); T4 T7.

---

### Stage I3. Hook-only attribution, injected origin and injector hints

**Context.** I-3 and INP-7.

**Algorithmic changes.**
- **Foreground timeline (§6.4).** The Context owner publishes ordered foreground, visit and focus-generation transitions with the tick uncertainty interval into a bounded Receiver timeline.
  - Only records in the I1 `HookOnly` state are approved through it. These are injected hooks, VK_PACKET without a raw twin, and unmatched hooks.
  - Keyboard records additionally require conditions 4–6 of §6.4 (a resolved, non-sensitive focus classification at receipt). Otherwise they take the redacted-aggregate path.
- **Origin (INP-7 precedence).** The order is: the promotion signature, then hook injection flags (`LLKHF_INJECTED`, `LLMHF_INJECTED`) giving `injected`, then device evidence.
  - A raw-only NULL-handle packet matched to an injected hook becomes `injected`.
  - Injected hooks are now eligible for pairing with NULL-handle raw packets of the same kind and identity, under the same unambiguity rules. This lifts the `unqualified` exclusion for that pair only.
- **Injector hints.** Serialize `injector_hints`. Values are the AutoHotkey extra-information range and the controller processes currently running from the CTX-12 settings snapshot, by hint name only.
- **Lateness.** `late` is null for injected hooks (CLK-3).

**Affected modules.** `core/src/platform/windows/context.rs` (transition feed), `receiver.rs` (timeline, bounded), `gate.rs` (`approved_receipt` for hook-only), `matching.rs` (`compatible`/`unqualified`), `evidence.rs` (origin, hints).

**Completion criteria (static).**
- A hook-only record has a defined approval path with conditions 1–3 of §6.4, and keyboard records also with conditions 4–6.
- A hook-only key in a password or possibly sensitive visit can never produce a per-key row.
- `injector_hint` is serialized.
- `origin:"injected"` is produced for flagged hooks.
- The timeline is pruned below the published prefix.

**Verification.** Execution: T4 T11 injected-Unicode (SendInput) scenario.

---

### Stage I4. Ordinary-key state across boundaries

**Context.** I-5 and ALG §4 ("not given a fabricated physical down; retain a marked boundary/unknown-state context").

**Algorithm.**
- **Resync snapshot.** At every resync, the existing safe-target check gates the read. The private resync snapshot covers all virtual keys (GetAsyncKeyState high bit), not only modifiers and buttons. It is used only for private state.
  - Ordinary keys get no synthetic edges, consistent with INP-10 and INP-13, which resynchronise only the modifier and button table.
  - Each key becomes privately `KnownUp`, `KnownDown(boundary)` or `Unknown(boundary)` (zero read while unsafe).
- **First down of a key after the boundary:**
  - `KnownUp` → a normal press: `repeat:false`, a pre-action trigger.
  - `KnownDown` or `Unknown` → published as a key down with `repeat:null` and `held_across_boundary:true`. It is not a press for gate 3 and does not trigger `pre_action`. The trigger is counted as not taken.
  - Later downs are repeats.
- **Key up without a published down** → published with `press_unavailable:true`, the same as buttons, instead of suppressed.
- **Text.** Text inference ignores `repeat:null` downs, because the press is unknown. Without layout evidence they are not typing.

**Affected modules.** `core/src/capture/input/held.rs`, `native-observation/src/held.rs` (snapshot width), `gate.rs` (`repeat`, `press_unavailable` for keys, trigger permission).

**Completion criteria (static).**
- No path publishes `repeat:false` for a key whose private state is Down or Unknown since the last boundary.
- `key_up` without a published down is never silently dropped.

**Verification.** Execution: T2 held tests; T4 pause while holding a key.

---

### Stage I5. Helper and Receiver liveness; non-fatal input faults; blind intervals

**Context.** Review §2.2 input rows, the opening-resync bound, non-ordinary foreground drops, unknown devices, blind rows at Stop only, and the detection part of D-2.

**Algorithmic changes.**
- **Producer stall.**
  - A producer is stalled when its progress has not advanced for 250 ms of QPC time while the controller is not paused, excluding suspend time.
  - Suspend time is computed from the S0 marks: the stall clock subtracts the overlap of `[last progress, now]` with `[suspend QPC, resume QPC]`. While a suspend mark exists without a newer resume mark, the stall rule is not evaluated. QPC counts through sleep, so without this exclusion every sleep would look like a stall.
  - I5 reads the marks through the read-only S0 accessor and has no dependency on S4.
  - A stall opens a blind interval at the last progress time, with reason `producer_stall`. Items in that range are resolved as in §6.3.
  - When progress resumes, the interval closes, a gap and resync are requested, and the blind row is emitted with its true start and end.
  - A stall lasting 5 s (REL-5 interval), or a helper process exit, is L-required for input (§6.2 rows 10–11). The helper is retired and the blind interval stays open until Stop. This is not a restart.
- **Opening resync.** If resync cannot be performed within 250 ms, because the foreground is inaccessible or the desktop is not the input desktop:
  - open a blind interval (`resync_unavailable`) from the boundary;
  - count dropped records (A1);
  - keep retrying on the existing 100 ms timer;
  - close the interval when resync succeeds.

  The unbounded silent drop is removed.
- **Non-ordinary foreground.** Raw records received while the receipt foreground is not ordinary-accessible:
  - are counted as `foreground_inaccessible`;
  - open a blind interval with the PDF reason `elevated_window` when the foreground is a higher-integrity process, otherwise `foreground_inaccessible`;
  - are not fed to the matcher; instead private held state for the interval is invalidated (a resync is required on return).
- **Malformed input.**
  - A Raw Input packet with a bad header or size, `RIM_TYPEHID` with a NULL device, or a failed `GetRawInputData` for one message, is counted (`malformed_packet`) and the loop continues.
  - Only a loop-level failure (window or registration API failure) is fatal to the helper.
  - In the helper output, a `PacketError` for one record is counted and the record skipped. A broken transport stays fatal (L).
- **Optional collections (INP-14, INP-15, M1-05).**
  - Touchpad and 3D-mouse collections are registered separately from the required ones. A failure marks that collection `unavailable` in the manifest.
  - Registration verification treats a missing optional collection as an `input_health` record plus manifest unavailability, not a helper failure.
  - The helper's notice of a lost optional collection is kept while the record queue is full and delivered when room returns, or at the latest at stop.
  - Optional HID traffic gets its own producer identity and loss slot. Its gap does not invalidate keyboard and mouse state.
  - This is a protocol enum extension and a helper change.
- **Required registration and hook loss (detection, under both D-2 options).**
  - A required registration mismatch on the 5 s check opens a blind interval with reason `registration_lost` and writes `input_health`.
  - HookHealth suspicion (20 unmatched transitions) opens a blind interval with reason `hook_lost` for the affected hook's evidence and writes `input_health`.
  - Under D-2 B, the interval stays open until Stop and the condition is L-required (input), §6.2 row 12. This matches the current path: at Stop, suspected hook loss is passed to `note_helper_failure` (`input.rs`, Stop branch after `take_hook_reports`). T4 should confirm the resulting disposition. Under D-2 A, stage I8 adds the repair.
- **Unknown devices.**
  - The helper sends `DeviceArrived` with the descriptor before the first record that uses a lazily assigned device ID.
  - The core inventory therefore resolves it, and SM_SWAPBUTTON and origin apply.
  - HID reports with an ID absent from the inventory are counted (`unknown_device`).
  - A device beyond the manifest inventory cap still reaches the Gate's touchpad inventory; only `device_inventory_truncated` is set.
- **Blind rows.** These are emitted when the interval closes, or at Stop for open intervals. Start is the earliest missing time (last progress or boundary), not the detection time. Placement is the context stream until R1 moves blind rows to `input.jsonl` (D-1).

**Affected modules.**
- `core/capture/native-observation/src/input/raw.rs`, `raw_loop.rs`, `device.rs`.
- `core/capture/input/src/windows.rs`.
- `protocol/src/ipc.rs` (optional HID producer; the `DeviceArrived` ordering contract).
- `core/src/platform/windows/input_helper.rs`.
- `receiver.rs`, `input.rs` (blind intervals, stall rule), `core/src/platform/windows/input/devices.rs`, `hook_health`.

**Completion criteria (static).**
- (Coordinator addition after the I5 fix review.) The INP-12 HookHealth watchdog does not count transitions whose receipt falls inside an open `producer_stall` interval of the affected hook producer, and its counter is reset when that interval closes. A stall shorter than 5 s therefore cannot escalate through row 12 (§6.2 rule 1).
- (Coordinator addition after the I1 review.) The fixed-kind `input_continuity_gap` rows that I1 kept for `hook_unavailable` and `device_removed` are replaced by typed blind intervals (reasons `hook_lost` and `device_removed`), and the fixed kind no longer exists. The start of a `producer_stall` range and of its blind interval is the producer's last progress, per §6.3, not the time of the first undecidable record. Per-second suppression and raw-motion counts are written in `input_health` rows, the PDF field that I1 left out.
- No per-packet error path returns a helper-fatal error.
- The stall rule excludes suspend time and has two thresholds: blind at 250 ms, L at 5 s.
- Suspend exclusion uses one S0-side query, "suspend-excluded elapsed time over [a, b]", that returns an explicit unknown (not zero) when a needed suspend or resume mark has no QPC; an unknown result never produces a fault and is treated as not evaluable. Direct subtraction from the latest suspend mark is not used (coordinator addition after the S0 review).
- The opening resync has a bounded blind-interval path.
- Optional collections cannot fail the helper.
- Blind start is the earliest missing time.

**Verification.** Execution: T4 T23 (flood the queue, kill the helper), sleep during recording, a HID device unplugged and plugged back.

---

### Stage I6. Native input hygiene

**Context.** ARCH-2 and the INP detail findings.

**Changes.**
- **Device worker.** Device descriptor IO (`CreateFileW`, `HidD_*`, `SetupDi*`) moves to a helper-owned device worker with a bounded request queue of 64.
  - The Raw Input thread uses the cached descriptor, or a pending ID that resolves when the worker replies.
  - Records carry the ID immediately, and `DeviceArrived` follows the I5 ordering contract.
  - Queue overflow is a typed loss. The descriptor stays unavailable for that device, which is recorded.
- **Foreground proof per WM_INPUT.** Cache the process identity per `(pid, creation time)`. Re-open the process only when the foreground PID changes. `GetGUIThreadInfo` and the layout read stay per batch, and the buffered drain already batches.
- **Hooks.** Replace the hook `thread_local RefCell` with an immutable reference set once before installation, such as a once-initialized static.
- **`LowLevelHooksTimeout`.** Read both REG_SZ and REG_DWORD. Clamp to the INP-12 maximum of 1,000.
- **HID unsigned values.** Read them with `HidP_GetUsageValue` whenever the scaled call returns a status other than SUCCESS or NULL. Signed values follow INP-11 sign extension.
- **Scan Time.** The first report of a new Scan Time base gets `source_t_us: null` and starts the fit (CLK-2: never copy receipt into source).
- **Absolute packets.** Serialize raw x/y (0..65535) and move flags beside the mapped point (INP-5).
- **Shared memory.** Accesses use raw-pointer volatile or atomic operations on slot fields, with a documented protocol invariant. No `&mut` reference to peer-shared memory remains.
- **`InputHelper::drop`.** The wait is bounded by the ordinary 750 ms cleanup bound, after the job close.
- **`pen_pressure`.** Map the opt-in-disabled state to the PDF enum (`unavailable_unknown`) with a separate `pen_hid_collection_enabled:false` extension field.

**Affected modules.** `native-observation/src/input/{device.rs,raw.rs,hooks.rs,hid.rs}`, `native-observation/src/input.rs`, `core/capture/native-observation/src/channel.rs`, `core/src/platform/windows/input_helper.rs`, `core/src/platform/windows/input/hid.rs`, `core/src/capture/input/evidence.rs`, `metadata.rs`.

**Completion criteria (static).**
- No `CreateFileW`, `HidD_*` or `SetupDi*` call is reachable from the Raw Input message handler.
- `OpenProcess` runs only on a PID change.
- No `RefCell` remains in the hook module.
- No `&mut` is taken over mapped shared memory.
- Each listed field change is present.

**Verification.** Static. Execution: T4 T20-partial on the available pen and hardware.

---

### Stage I7. Text commit status and commit loss accounting

**Context.** INP-3, review §2.1 text and context commit losses.

**Changes.**
- **Dependency.** I7 depends on C1 only for the IME `unknown` state (C1 replaces `not_ime` without a snapshot). The `typing:"unknown"` cases used below exist in the current code (Editable without layout evidence). D-4 can add more such cases through C2. I7 does not depend on C2: the rule applies to whatever cases classification yields.
- **`text_commit` status `unknown`.** It is written:
  - when typing is Unknown in a non-sensitive field and key evidence suggests text;
  - when the IME state is unknown while typing (INP-3: "text typed meanwhile is status: unknown");
  - for Alt+Numpad entry, which M1 does not rebuild (INP-3 allows "or write status unknown").

  The row has `text:null`.
- **Late or invalid commits.** They are recorded:
  - `late_commit`, `commit_capacity` (the 4-slot UIA ring; `take_loss` is now called on the ordinary path), `commit_too_long` and `commit_parts` as typed losses (counts and ranges, no text);
  - invalid UTF-16 becomes `text_commit` with `status:"invalid"` and U+FFFD replacement, as for VK_PACKET.
  - In a sensitive field, losses are only counted into the redacted aggregate.
- **`future_text` cap overflow.** It is a typed loss.

**Affected modules.** `core/src/platform/windows/input/text.rs`, `gate.rs`, `native-observation/src/uia_commits.rs`, `core/src/platform/windows/context.rs`.

**Completion criteria (static).**
- `TextStatus` has `Unknown`.
- Every early return in commit intake records an outcome.
- `take_loss` is called on the ordinary path.

**Verification.** Execution: T4 T11 (IME, dead keys).

---

### Stage I8. Registration and hook repair (blocked by D-2; only if option A)

**Algorithm.**
- **Registration.** On a required registration mismatch, re-register the same collections from the Raw Input thread. Close the blind interval at the first record after successful verification, and write `input_health`.
- **Hooks.** On hook-loss suspicion, the hook thread uninstalls and reinstalls that hook on its own thread. The blind interval closes at the first matched record.
- **Bound.** At most one repair per 5 s per kind. A second failure of the same kind within 5 s is L-required (input), §6.2 row 12.

**Affected modules.** `native-observation/src/input/{raw.rs,hooks.rs,raw_loop.rs}`.

**Completion criteria (static).** Repair is rate-bounded, recorded, and the only mutation is re-registration or reinstallation of the existing set.

---

### Stage C1. Focus read corrections that do not change disputed redaction

**Context.** C-1 (c), INP-2 fallback, typed timeouts. The critic review showed that some C-1 changes reduce redaction in cases ALG §7.1 treats as unresolved. Those changes (classification of `editable:null`, pattern-less elements, `HasKeyboardFocus`) are moved to D-4 and C2. C1 keeps only corrections that follow CTX-9 literally or that never reduce redaction.

**Changes.**
- **`editable` evaluation.** Evaluate both patterns as tri-state values (writable, read-only, unavailable or mixed). The result is:
  - **true** if IsKeyboardFocusable is true and at least one pattern reports writable (CTX-9: "either … is false");
  - **false** if IsKeyboardFocusable is false, or if every present pattern reports read-only;
  - **null** in every other case.

  This removes the `ValueIsReadOnly=true` early return that skipped a writable Text pattern.
- **Classification of these results.** The mapping does not change in C1. `editable:true` is Editable. `editable:false` is NonText. `editable:null` stays PossiblySensitive (redacted) until D-4 and C2 decide otherwise.
- **Redaction direction.** The only change in redaction is in the conservative direction. A field whose Value pattern is read-only but whose Text attribute is unavailable used to be NonText (keys published); it becomes null, which is redacted.
- **Focus snapshot.** Use `hwndActive` when `hwndFocus` is NULL (INP-2, both for the layout and the focus snapshot).
- **UIA status.** UIA_E_TIMEOUT and a deadline expiry map to `TimedOut`.
- **IME state.** Without a focus snapshot, the IME state is `unknown`, not `not_ime`.
- **UIA call budget.** Before each provider call, set the client transaction timeout to the remaining request budget, capped at 500 ms. If the remaining budget is below a minimum useful call time, stop and return TimedOut. The total therefore never exceeds the 500 ms deadline.

**Affected modules.** `native-observation/src/uia.rs`, `native-observation/src/windows.rs`, `core/src/platform/windows/context/{queries.rs,ime.rs}`.

**Completion criteria (static).**
- `editable()` has no early return on `ValueIsReadOnly` before the Text pattern is checked.
- The classification mapping for null, false and true is unchanged from the current code.
- The per-call timeout is derived from the remaining budget.

---

### Stage C2. Classification of undetermined editability (blocked by D-4)

**Algorithm (recommended combination a1, b1, c1, d1+d2).**
- **D-4a.** A successful read with IsKeyboardFocusable known true and neither the Value nor the Text pattern present gives `editable:false` (NonText; keys preserved). This applies to every control type, and the fixed type list is removed.
- **D-4b.** A present pattern whose read-only attribute is unavailable gives `editable:null` with sub-reason `attribute_unavailable`. It stays PossiblySensitive (redacted visit).
- **D-4c.** A mixed Text attribute gives `editable:null` with sub-reason `mixed`. When IsPassword is known false, the classification is TypingUnknown: keys are published with `typing:"unknown"` and no `text_commit` inference.
- **D-4d.**
  - `HasKeyboardFocus` false: the read is stale. Re-issue it once within the classification wait; if the wait expires, redact.
  - `HasKeyboardFocus` unsupported: the read is treated as resolved when the element came from the focused-element query and the native focus snapshot confirms the same window.
- **Unchanged.** Read failure or timeout, IsPassword unknown, and password always lead to a redacted visit.
- **Value pattern presence unknown (coordinator note after the C1 review).** When `IsValuePatternAvailable` is itself unavailable and `ValueIsReadOnly` is unreadable, C1 treats the Value pattern as absent, which is the old fall-through. Under D-4a this case would publish keys as NonText, so it must be classified explicitly. Under b1 it is an unavailable attribute of a possibly present pattern: `editable:null`, sub-reason `attribute_unavailable`, redacted visit.
- **Focus snapshot from `hwndActive` (C1 fix).** A snapshot taken from the `hwndActive` fallback never serves as password evidence. Its `password_style` is unknown, and D-4d's "native focus snapshot confirms the same window" requires a snapshot taken from `hwndFocus`, not from the fallback.
- **Other choices.** Any other combination chosen in D-4 maps onto the same four decision points.

**Affected modules.** `native-observation/src/uia.rs` (`editable`, classification, a `TypingUnknown` classification value), `core/src/platform/windows/context/queries.rs`, `gate.rs` (publication for TypingUnknown).

**Completion criteria (static).**
- Each of the four cases is handled at one identifiable decision point, with the outcome chosen in D-4.
- No control-type list remains in `editable()` (under a1).

---

### Stage C3. Context loss accounting, non-fatal identity, UIA worker diagnosis

**Changes.**
- **Typed losses (A1).**
  - Clipboard notice, grant and result queue drops (`clipboard_queue`, counts only; content is never recorded).
  - `clipboard_notices`, `future_clipboard`, `cursor_completions` caps.
  - Cursor asset queue full or `try_send` failure: the `cursor` row carries `custom_asset:"unavailable"` with reason `cursor_asset_queue`, distinct from "not needed". The shape is retried on the next observation of the same cursor handle.
  - Notification ring overflow becomes a typed gap with the first and last receipt of the lost notifications. The ring records the receipt of the first and last dropped notice. During a collector-backed visit, a ring loss that ended before the visit opened is only counted (G, §6.2 row 18). A later one may have hidden a foreground change, so it revokes the visit from its first possible notice (a source change), instead of failing the source.
  - `context_notifications_gap` becomes a typed gap.
- **WinEvent callback.** Filter top-level kinds (NAMECHANGE, LOCATIONCHANGE, SHOW, HIDE, DESTROY) with `GetAncestor(hwnd, GA_ROOT) == hwnd` in the callback (CTX-1). `GetAncestor` is a cheap, non-blocking user32 query; no lock or allocation is added.
- **Identity resolution.** A canonicalization failure or a resolution pending over 2 s makes that foreground approval-unknown: suppressed, a G loss, plus a diagnostic report entry. `Context::poll` no longer returns Err for it. A resolver thread blocked in one native call longer than the 2 s bound is L-auxiliary (§6.2 row 15): later alternative spellings stay unknown, and no replacement thread is started.
- **UIA worker.** A client open failure, or a worker blocked in one call for more than two transaction timeouts (1 s), writes one health row per episode and is L-auxiliary for UIA (§6.2 row 14). It does not change the disposition. Requests continue to resolve as `unavailable`/`timeout` per request. No replacement worker is started (ALG §7.1).
- **Clipboard over 64 KiB.** It produces `clipboard_change` with `text:null` and reason `too_large`, no content.

**Affected modules.** `core/src/platform/windows/{clipboard.rs,context.rs}`, `context/{identity.rs,uia.rs,cursor.rs}`, `native-observation/src/events.rs`.

**Completion criteria (static).**
- Every `try_send` result in the context and clipboard owners is checked and accounted.
- `Context::poll` has no Err path for identity resolution.
- The GA_ROOT filter is in the callback.
- The UIA worker has a health path.

---

### Stage C4. Context fidelity

**Changes.**
- **Ghost windows (IMG-8, PRIV-2).** A Ghost window that has verifiably replaced a hung approved window (the existing hung-tracker association) counts as the approved app for foreground approval and frame admission. UIA reads to it are disabled, and its row carries `ghost_replacement:true`. An unresolved Ghost stays unapproved. Under per-process approval (SPEC addendum A7.8) a foreground notice for a Ghost window still interrupts the approval: a new frame-approval generation starts even when the Ghost counts as the approved app.
- **LOCATIONCHANGE.** The limiter keeps per-window "dirty" state. When the 100 ms window ends with suppressed notices, re-read the window geometry once (trailing edge) and update the visit geometry.
- **Hint records.** MINIMIZESTART/END, SWITCHSTART/END and IME_SHOW/HIDE/CHANGE produce typed context hint records (window identity only for approved windows), as for the existing menu and dialog hints.
- **Pointer reads.** A click while a pointer query at another point is pending gets an element link with reason `pointer_query_busy`. A click at the same point links that pending read when it completes, before the click's own deadline.
- **CTX-12.** Record the pen and tablet driver version from the device driver properties. This is read on the device worker (I6), not the Raw Input thread. Mode is `unknown` unless observed (INP-11 permits unknown).
- **ES_PASSWORD.** Apply it only to Edit-class and RichEdit-class windows. Other classes rely on UIA IsPassword.
- **Metadata race.** When `WindowMetadata::read` fails because the identity changed during the read, re-read once. If it fails again, write `approved:true` with metadata `unavailable` instead of `approved:false`.
- **`hung.rs` transmute.** Add the safety invariant (the exported signature `HWND → HWND`, stdcall) as a SAFETY comment. This is documentation only.

**Affected modules.** `context.rs`, `context/{hung.rs,frame_policy.rs,queries.rs,settings.rs}`, `native-observation/src/{windows.rs,hung.rs,events.rs}`.

**Completion criteria (static).** Each item is present, with no blocking call added to any callback.

---

### Stage C5. Unapproved-foreground intervals (blocked by D-3)

**Option A.**
- **Suppression intervals.** The Context owner records each suppression interval: start = the conservative foreground change time, end = the return of an approved foreground.
- **Accounting.** The input gate counts suppressed records per interval (counts only). At interval end it writes one `suppressed_interval` record with reason `non_approved_app` (or `unknown_foreground`) and counts per stream. No identity is recorded.
- **Manifest.** The health block lists the interval count and total duration.

**Option B.** Add the `NonApprovedApp` and `AlwaysPauseWindow` pause reasons. These are automatic: they set and clear with the foreground. They use the existing pause and resume path with segment boundaries, plus pause and resume rows with slot ranges. `late_pause` is not written (M3).

**Affected modules.** `context.rs`, `gate.rs`, `session.rs`/`session/windows.rs` (B only), `metadata.rs`.

---

### Stage V1. FPS-derived video capacities

**Context.** V-1.

**Changes.**
- **Staging bounds.** `MAX_SEGMENT_PICTURES` and `MAX_STAGED_SLOTS` become per-encoder values: `(segment duration + rotation allowance) × effective fps + margin`.
  - The rotation allowance is the existing 1 s IDR wait.
  - The effective fps is the qualified encoder fps.
  - The values are computed when the encoder format is selected and recorded in the segment metadata.
- **Evidence ring.** The size is at least the staged slot bound.
- **Frames journal capacity.** At least the maximum number of rows that can be pending publication: `selected monitors × staged slot bound` plus boundary reserve. It is computed at start from the selected monitors and the requested fps.
  - With the §6.5 merge (D-1 A), the capacity is reserved per monitor lane. Each monitor's lane plus its worker staging hold at least twice the staged slot bound plus `ceil(T × effective fps)` rows, where T is the §6.5 row-21 watchdog bound. One monitor's rows never consume another's capacity, and they fit while waiting up to T for another monitor's mark.

**Affected modules.** `core/src/platform/windows/{video.rs,slots.rs}`, `core/src/package/journal.rs` (capacity parameter).

**Completion criteria (static).**
- No video capacity constant contains a literal fps.
- All bounds are derived from the effective format.
- The frames journal is still written only by blocking submission. Raising its capacity introduces no loss path (§6.1 rule 6).
- With the §6.5 merge, the per-monitor lane plus staging capacity is computed as 2 × staged slot bound + `ceil(T × effective fps)`, with T the row-21 bound. No literal is used for any term.

**Verification.** Execution: a T4 60 FPS run longer than 60 s on qualifying hardware.

---

### Stage V2a. Repeat-source continuity and timeline-pending accounting

**Context.** The timeline-unavailable bullet and the DPI or position repeat reset. Neither changes the frame-to-slot rule.

**Algorithm.**
- **Timeline after Resume.**
  - Frames arriving before the epoch timeline is published are held in a bounded one-frame "pending timeline" slot.
  - They are submitted when the timeline arrives, if still eligible under the current acceptance rule. Otherwise they are counted as `timeline_pending`.
  - The slot reason for an affected slot is `timeline_pending`, not `BeforeFirstFrame`.
- **Repeat across geometry changes.**
  - A geometry epoch change that keeps the pixel size and format (DPI or position, ALG §9.3: "update geometry metadata without recreating the pool or segment") does not clear `previous`.
  - Repeats across it keep the frame's original geometry version.
  - Size, rotation and format changes still clear it.

**Affected modules.** `core/src/platform/windows/{video.rs,monitor.rs}`, `slots.rs` (reasons).

**Completion criteria (static).**
- `previous` survives same-size epoch changes.
- No frame-skip path lacks a counter.

---

### Stage V2b. Slot decision watermark (blocked by D-10)

**Context.** V-2. This is an algorithm amendment to the ALG §9.1 decision point: option A of D-10, with sources VID-4 (slot membership by source time) and ALG §9.1. Stage D1 records it in the algorithms addendum once D-10 is accepted.

**Algorithm (D-10 option A).**
- **Deciding a slot.** Slot k `[start_k, end_k)` is decided when either:
  - the capture owner has delivered a frame with source time ≥ `end_k` (WGC delivers frames in source order, so no later frame can belong to k); or
  - the decision grace has passed: `now ≥ end_k + G`.
- **Grace.** `G` = one capture pacing interval + the maximum conversion latency observed bound, which is the existing per-conversion deadline capped at two slot intervals. `G` is recorded in the manifest as an engineering setting.
- **Frame eligibility.** A frame with source in `[start_k, end_k)` is eligible for k whenever it arrives before k is decided. `recv_t_us < end` is no longer required, but `available` must precede the decision.
- **Late flag.** `late` is unchanged (VID-4). Frames arriving after their slot was decided are counted as `arrived_after_decision`.
- **Queue bound.** `G` adds at most two frames of buffering. The video input queue bound of 3 outstanding samples (ALG §9.2) is unchanged, because selection happens before submission.

**Affected modules.** `core/src/platform/windows/{video.rs,monitor.rs}`.

**Completion criteria (static).**
- The decision condition is "source watermark ≥ end or grace elapsed".
- `recv_t_us < end` is no longer an acceptance condition.
- `G` is recorded in the manifest.

**Verification.** Execution: a T4 static-desktop single-change scenario (count `arrived_after_decision`), plus T3.

---

### Stage V3. Segment verification off the video worker

**Context.** V-3; prerequisite for D-5.

**Algorithm.**
- **Rotation on the video worker.** The worker keeps the steps the ALG mandates:
  - IDR request and verification;
  - `Finalize`, release and `FlushFileBuffers` of the old `.part`;
  - opening the next part with the held IDR.

  It then hands a `SegmentToVerify{path, expected index}` to a per-attempt **segment verifier** thread over a bounded queue of capacity 2 (one running plus one queued). It resumes encoding immediately.
- **Verifier.**
  - Performs the existing decode verification, requesting a hardware decoder through the D3D manager when available and otherwise using software.
  - On success, renames to `.mp4`, with bounded sharing-violation retries inside the Stop deadline.
  - Returns `SegmentVerified{segment, published bytes}` or `SegmentRejected{cause}`.
- **Frame rows.** Rows for a segment are resolved when its verification result arrives. Staged slot rows stay in the bounded staging of V1, which therefore covers two segments. The V1 bound is doubled for this purpose.
- **Verifier queue full.** If a third segment closes while two are pending, the video worker waits for a verifier slot. Capture continues, and slots are recorded as `QueuePressure` (ALG §9.2). Each wait is a health counter.
- **Rejected segment.** It is L-required for that monitor's video (§6.2 row 9), as now. The `.part` is preserved.
- **Watchdog.** The encoder watchdog no longer sees the long synchronous decode, so the false trigger is removed.
- **Stop.** Stop waits for pending verifications within the Stop deadline. An unverified segment at the deadline stays `.part` and is reported as a required finalization failure (F), as now.

**Affected modules.** `core/src/platform/windows/{video.rs,mp4.rs,mf.rs,encode.rs}`, `platform/windows.rs` (Stop joins).

**Completion criteria (static).**
- (Coordinator addition after the V2a review.) The reconfigure precondition ("all staged rows published", `video.rs` before reconfigure) does not turn an ordinary format change during a pending asynchronous verification into F. Either the transition waits for verification within the allowance, or the pending rows are carried with their original bound.
- `verify_decoded_frames` is not reachable from `output()` or the encoder callback path.
- The verifier queue is bounded at 2.
- Renaming happens only after verification. The rename-back-to-`.part` path is removed.

**Verification.** Execution: T4 T1 (segment rotation under load, gate 2).

---

### Stage V4. Non-fatal display and encoder selection; self-check placement

**Changes.**
- **Display enumeration.**
  - A per-monitor read failure marks only that monitor's fields unavailable (null DPI, SDR white or refresh, with a reason).
  - An output whose color mode or rectangle cannot be read fails only the resolution of that monitor. It proves neither presence nor absence, so a full enumeration with such an output fails and is retried on the next topology tick.
  - A failure for an unselected monitor never affects capture.
  - For a selected monitor, the reconfigure path keeps the old epoch closed and records `display_reconfiguring`. It retries on the next topology tick, so slots become `MonitorMissing`.
  - Topology-race exhaustion is retried on the next tick.
  - None of these return Err to the capture thread (VID-9).
- **HDR without an SDR white level.** Use the B8G8R8A8 capture path with `conversion:"system_bgra8"` and a health row (VID-8).
- **Encoder reselection after a format change.** Record every rejected hardware candidate with its cause (health row plus segment metadata). When no encoder qualifies and the selection fails, the health rows are still written. The SW selection remains a VID-6 capability selection for the new format, recorded as such.
- **Self-check during reconfigure.** The conversion steps of the self-check (`VideoProcessorBlt`, `CopyResource`, `Map`) run on the capture thread as part of handling the reconfigure request. The capture thread produces the self-test samples, and the video thread drives only the encoder (VID-12).
- **SW path.** Convert and read back only the frames selected for video slots at the encoding fps. Thumbnails and still selection stay at capture pace.
- **Bitrate metadata.** Rename to `configured_peak_bitrate` and `configured_mean_bitrate`.

**Affected modules.** `core/src/platform/windows/{display.rs,monitor.rs,video.rs,encoder.rs,gpu.rs}`, `display/topology.rs`.

**Completion criteria (static).**
- `display::enumerate` errors cannot reach `request_stop`.
- No immediate-context call is reachable from the video thread.
- Candidate causes are recorded.

---

### Stage V5. Suspend finalization (blocked by D-5)

**Algorithm (option A).**
- **On PBT_APMSUSPEND.** The controller enters Paused(Sleep) and requests a pause boundary as now. It allows 2 s for video producers to report that their open segment is closed (Finalize, flush, handed to the verifier).
- **On expiry.** The controller records a G health row, `suspend_finalize_late`, listing the monitors. It returns from the suspend wait without failing. The video worker continues, after wake if necessary, and reports normally. Verification can always complete after wake.
- **Deadline sources.** `segment_deadline` is no longer used by `mp4::check` or the video `observe_admission` for suspend. Only the Stop and session-end deadlines bound those calls.
- **Elapsed time.** It is measured on QPC, excluding the interval between the S0 suspend and resume marks, so sleep duration never counts.

**Affected modules.** `core/src/session/windows.rs`, `session/windows/environment.rs`, `video.rs`, `mp4.rs`.

**Completion criteria (static).** No suspend-related deadline can produce an attempt failure; only Stop and session-end deadlines can.
- (Coordinator addition after the Track A batch 1 review.) The segment verifier no longer uses the 2 s suspend part of `segment_deadline`. A slow verification during suspend does not become an L video loss.
- Suspend exclusion uses one S0-side query, "suspend-excluded elapsed time over [a, b]", that returns an explicit unknown (not zero) when a needed suspend or resume mark has no QPC; an unknown result never produces a fault and is treated as not evaluable. Direct subtraction from the latest suspend mark is not used (coordinator addition after the S0 review).

---

### Stage S0. Suspend and resume QPC marks; environment callback hygiene

**Context.** I5, S1 and V5 must exclude sleep from QPC-based stall and deadline measurements. Today the power callback (`session/windows/environment.rs:280`) stores only a suspend `Instant` behind a Mutex. `PBT_APMRESUMEAUTOMATIC` clears the Sleep reason but records no time.

**Changes.**
- **Marks.** The power callback stores the QPC of `PBT_APMSUSPEND` and of `PBT_APMRESUMEAUTOMATIC` in two atomics, each paired with a suspend sequence number (an atomic counter incremented on suspend) so that readers can detect a suspend they missed. This replaces the Mutex-protected `Instant`. The controller's existing `segment_deadline` logic reads the suspend QPC from the atomics.
- **Accessor.** A read-only accessor on the worker session returns `(sequence, suspend QPC, resume QPC or none)`. The suspend-excluded elapsed time over an interval is computed from these values.
- **Callback hygiene.** S0 owns all callback hygiene in `environment.rs`. Both the power callback and the `desktop_changed` WinEvent callback run their bodies inside `catch_unwind`, and neither takes a lock.

**Affected modules.** `core/src/session/windows/environment.rs`, `core/src/session/windows.rs` (accessor, `segment_deadline` source).

**Completion criteria (static).**
- No `Mutex<Option<Instant>>` (or any other separate suspend store) remains. The atomics behind the accessor are the only suspend and resume source.
- No Mutex is taken in either environment callback, and both contain `catch_unwind`.
- Both marks are written.
- The accessor is read-only.

---

### Stage S1. Hotkey liveness and Stop availability

**Changes.**
- **Overdue progress.** A hotkey progress gap over 250 ms, excluding suspend time (S0 marks), records a `ControlLoss` interval as now. It does **not** latch `hotkeys.failed`. Activations after the gap are processed normally.
- **Unavailability.** `hotkeys_unavailable` is shown only while the hotkey thread is actually not running, or a registration was lost. Stop via the UI and window close always remains available.
- **Lost registration.** A lost registration during recording records health and shows the UI status. Re-registration follows the existing "Retry hotkey registration" control.

**Affected modules.** `core/src/session/windows.rs` (hotkey section), `core/src/platform/windows/hotkeys.rs`, `ui/src/app.rs`.

**Completion criteria (static).** No permanent latch discards later activations.
- Suspend exclusion uses one S0-side query, "suspend-excluded elapsed time over [a, b]", that returns an explicit unknown (not zero) when a needed suspend or resume mark has no QPC; an unknown result never produces a fault and is treated as not evaluable. Direct subtraction from the latest suspend mark is not used (coordinator addition after the S0 review).

---

### Stage S2. UI and publication outcome corrections

**Changes.**
- **Admitted non-elevated failures.** These show the retained failed package when its manifest publication was confirmed, keep `close_after_recording`, and are distinct from preflight errors (M1-01).
- **Indicator diagnostic.** It is taken on both the Ok and Err branches.
- **Countdown blocked by lock or secure desktop.** The UI shows the blocking reason. The countdown restarts from its configured length when the reasons clear, because the PDF requires the approved app in front at countdown end.
- **Manifest deadline.** `check_deadline` is not evaluated after a successful authorized `MoveFileExW`. The Windows result is final (C6).
- **Still failures.** PNG write failures and other still losses produce the final message "Recording complete with recorded losses (N stills not saved)" instead of an unqualified "Recording complete" (C6).
- **Blind labels.** Blind intervals during an elevated-epoch pause are labelled with the actual pause reasons. `elevation_transition` is used only for the transition latch itself.

**Affected modules.** `ui/src/app.rs`, `core/src/platform/windows.rs`, `core/src/package/metadata.rs`, `core/src/session/windows.rs`.

**Completion criteria (static).** Each listed UI branch and message is present, and no post-replace deadline check remains.

---

### Stage S3. Completion rule for selected monitors (blocked by D-6)

**Option A.**
- The disposition computes per monitor the set of present-and-admitted intervals from the monitor epochs and the missing-slot reasons.
- `VideoUnavailable` is chosen only when a monitor has an L-required video fault, or has a present-and-admitted interval with no encoded slot.
- Slots with `MonitorMissing` do not affect Complete.

**Affected modules.** `core/src/package/metadata.rs` (disposition, `validate_recorded_inventory`).

---

### Stage S4. Controller-owned facts, explicit state machine

**Context.** Architecture addendum §4 and C5 violations. This stage comes after A2, so the fault path is already value-based.

**Changes.**
- **Value-only workers.** Workers send `FaultReport`, `PauseFinished`, `VideoReady`, `DiskSample`, `SourceCoverage` and `InputPrefix` as values over one bounded controller channel. It reserves capacity for terminal and boundary messages, as the architecture addendum §5 requires.
- **Read-only snapshots.** The controller alone mutates `Facts`. Workers read an immutable snapshot (cloned values or atomics written only by the controller).
- **One controller-internal state value.** It uses the existing public phases (Starting, Recording, Paused, Stopping, Finished, Failed). Associated data holds the pause-reason set, the stop request, the source-change latch, the clock state and the publication authorization. These replace the separate `stop`, `source_change`, `clock_failed` and `complete_publication` latches and the `coordinate` locals.
- **No Idle phase.** The public `SessionPhase` is unchanged. No `Idle` variant is added, because no defect requires one: the UI already represents Idle and the controller does not exist before Start.
- **Stop request.** Stop requests from workers become messages, and the controller sets the state.
- **Suspend data.** S4 only switches the controller's internal consumers to the S0 accessor, where any remain. Callback hygiene is entirely in S0, and S4 does not change the callbacks.

**Affected modules.** `core/src/session/windows.rs`, the worker call sites in `platform/windows.rs`, `video.rs`, `input.rs`, `monitor.rs`. The public `core/src/session.rs` API does not change.

**Completion criteria (static).**
- (Coordinator addition after the Track A batch 1 review.) The health records that workers write into `Facts` through `report_health` move to the owner/message model together with the other worker-mutated facts.
- `WorkerSession` exposes no mutating method on shared facts.
- One controller-owned value holds the lifecycle state and latches.
- Every suspend consumer reads only the S0 accessor. The environment callbacks are unchanged by S4.
- The S0 accessor contract is preserved.
- `window_proc` (WM_ENDSESSION, power and other notifications) runs its body inside `catch_unwind`, which S0 did not cover. The wake path does not depend on a lock inside the callbacks (coordinator addition after the S0 review).

**Verification.** Execution: T4 pause, resume, stop and sleep scenarios.

---

**Scope reduction (coordinator, under the user's directive to simplify, 2026-11-17).** The full refactor of S4 is deferred and recorded as a known architectural deviation in D1:
- moving every worker-mutated fact (about 40 `WorkerSession` methods, `report_health` included) to a bounded message channel;
- merging the lifecycle flags into one state value.

The reason is size and risk against the minimal-code directive. The refactor would rewrite `session/windows.rs` and every worker call site, while it fixes no observable defect.

S4 keeps only:
1. `catch_unwind` in `window_proc`;
2. a single place that documents the lifecycle states and the latch flags as the explicit state machine (no new types);
3. the suspend consumers on the S0 accessor (already done).

S1's "lost registration records health" item is dropped as well: Windows gives no signal for a lost `RegisterHotKey` registration, so detecting one would mean adding new polling behaviour.

### Stage P1. Still drop accounting and reasons

**Changes.**
- **PNG inbox.** The inbox uses a short blocking lock: the worker holds it only to pop. `Capacity` is reported only when the queue is actually full.
- **Correct reasons:**
  - checkpoint without an eligible interval: `save_interval_changed` or `admission_closed` by cause;
  - `rest()` failures: `admission_changed`, `geometry_changed` or `frame_not_approved`;
  - `PrivateSelection::Unavailable`: `no_approved_interval` or `adjustment_failed`.

  The fallback reason is used only for an actual handoff disconnect.
- **PNG write failure.** One context health row per failure kind episode (A1). The `still_dropped` row keeps the operation and native code. Totals go to the manifest health `dropped_stills`.
- **Cursor assets.** The cursor asset loss path follows C3.
- **PNG worker exit (§6.2 row 16, coordinator amendment after the A2 review).** Today a PNG worker exit is F (`platform/windows.rs`, component stills). Under row 16 it becomes L-auxiliary (stills):
  - the worker exit is reported once as an activity loss with its cause;
  - pending and later still requests resolve as `still_dropped` with reason `png_worker_unavailable`;
  - recording continues;
  - the disposition is not changed by this loss (rule 4).

  A stills journal write failure remains F under row 1. When the worker exits because its journal failed, the F from row 1 is the reported cause, and no second L report is added for the same event.
- **Drain.** `expire_drain` marks the corresponding PNG jobs cancelled through a per-job cancellation flag shared with the worker. The worker checks it before writing and before the rename. A file already renamed before cancellation is reported back. The row is then published as a success if its slot is still open; otherwise the file is kept and not deleted (A2: do not delete saved data). The PNG counter `unreferenced_images` counts exactly every inventoried image that no journaled still row names in `file` or `seen_file`, plus revoked images whose removal failed.

**Affected modules.** `core/src/platform/windows/{still_png.rs,stills.rs,still_rows.rs,context/cursor.rs}`.

**Completion criteria (static).**
- No `try_lock` rejection path remains.
- Each listed reason is produced at its cause.
- Cancellation is checked before write and rename.
- PNG worker exit not caused by a journal failure produces an L-auxiliary (stills) report and no F. A stills journal failure still produces F. After the L-auxiliary report the UI shows the degraded activity, and the disposition stays `recorded`.

---

### Stage P2. Retention fairness and trigger correctness

**Changes.**
- **PreMotion envelopes.** A PreMotion provisional envelope, with its EarlyRows reservation, opens only for the first motion after at least 300 ms without motion (the IMG-2 trigger condition), evaluated at receipt in the Receiver. Other motion parts reserve nothing.
- **Private holds.** Pending private provisional selections participate in `reclaim` by IMG-7 priority and age. Among equal priority, the newest is evicted first, consistent with REL-2 and section 4. The total number of private holds per monitor is capped at 2 of the 3 extra holds, so one hold is always available to a public request.
- **`start` trigger.** It checks `last_evicted` and the capture and approval epoch. It returns `history_lost` instead of a later frame (ALG §8.2).
- **Scroll without a device.** Ticks without device attribution form one "unknown device" burst per monitor and axis (ALG §8.3: known devices are never merged with each other).

**Affected modules.** `receiver.rs`, `stills.rs`, `still_time.rs`, `native.rs`.

**Completion criteria (static).** Each rule is present at the cited decision point.

---

### Stage P3. Shared physical image work per retained frame

**Context.** C2 ("shared as immutable image data rather than copied for each reason").

**Algorithm.**
- **One readback per held frame.** The still owner keys readback work by the held frame identity, not by logical slot. The first slot that needs the frame's pixels schedules one readback; later slots for the same held frame attach to that work item.
- **One PNG job per frame.** When the CPU pixels are ready, one PNG job is created per frame. On completion, the result (digest, file or failure) resolves every attached logical slot.
- **Lifetime.** The work item is released when the last attached slot resolves. A slot cancelled while attached only detaches.
- **Budget.** Bytes are charged once per frame.
- **Staging texture.** Reused per capture geometry epoch (size and format). It is recreated only on an epoch change or `cancel_epoch`.

**Affected modules.** `stills.rs` (`work()`), `readback.rs`, `still_png.rs` (job → slots fan-out).

**Completion criteria (static).**
- Readback submission is keyed by held frame.
- The staging texture is not dropped after each map.

---

### Stage P4. Output inventory for images and cursors

**Changes.**
- **Inventory.** The PNG and cursor asset owners report each successfully published file (digest, relative path, bytes, dimensions) to the metadata owner. The manifest lists `raw/images/` and `raw/cursors/` entries (M1-09 "output inventory").
- **Reader.** `artifact_path` resolves only inventoried images and cursors. Stills and cursor rows must reference inventoried files.

**Affected modules.** `still_png.rs`, `context/cursor.rs`, `metadata.rs`, `reader.rs`.

---

### Stage E1. Standard-source continuity during foreground classification

**Context.** E-2.

**Rule.** "Classification pending" does not mean "inactive". Until the selector has decided that the collector is required, and has set the source-change latch at the conservative detection time, the standard source remains selected. Its Input, Progress and Gap records continue into the Receiver. Approval is decided per receipt by the gate, and records under a non-ordinary foreground are handled by I5.

The standard source becomes inactive only at the latch. Records with a receipt after the latch time are drained without feeding the Receiver, counted, and covered by the transition blind interval (ELEVATED §4.2).

**Affected modules.** `core/src/platform/windows/elevated/selector.rs` (`ordinary_allowed`), `input.rs:253-258`, `receiver.rs` (`collect_ordinary_inventory` call conditions).

**Completion criteria (static).** No helper record is discarded while the classification is `None` or `Unknown`, unless it is after a latched transition.
- (Coordinator addition after the I1 review.) When an elevated visit is interrupted, records after `through` in the same epoch are counted with a typed reason; they are not dropped silently.

---

### Stage E2. Collector provider deadlines

**Context.** E-3; ELEVATED §3.3 and §5.2.

**Algorithm.**
- **Request deadline.** Each read has the request deadline from the Recorder: UIA ≤500 ms, IME ≤50 ms. The provider's own Windows timeouts are bounded: the UIA transaction timeout is set per call to the remaining budget (as C1), and the IME `SendMessageTimeoutW` timeout is the remaining budget.
- **Supervisor slot deadline.** Request deadline + 750 ms cleanup bound.
- **Request deadline expiry.** If the call is still running at the request deadline, the collector immediately sends the typed `TimedOut` result for that request and stops dispatching new calls to that provider kind. The grant stays valid.
- **Slot deadline expiry.** If the call has still not returned at the slot deadline, the provider defeats cancellation (§5.2). The supervisor revokes and terminates, as now. That is a collector failure for the Recorder and therefore an attempt failure (F, §6.2 row 6).
- **Late return.** A call that returns between the two deadlines is discarded as stale. The provider kind becomes available again.
- **Termination is mandatory.** ELEVATED §3.3 requires it: "if a revoked call/worker fails to join before the cleanup deadline, forcibly terminate this process from the supervisor with `TerminateProcess`". §5.2 says "a provider that defeats cancellation is terminated by the supervisor". ELC-5 and the §4.2 last row make an admitted privileged-source failure a failed attempt.
- **What this stage closes.** E2 therefore closes finding E-3 only partially. It removes the zero margin, so a slow but working provider that returns within its own timeouts no longer ends the attempt. A provider that hangs beyond request deadline + 750 ms still terminates the collector and fails the attempt, as the contract requires. Making that non-fatal would need a change to ELEVATED §3.3 and ELC-5, which this plan does not propose.

**Affected modules.** `core/capture/elevated/src/runtime/{context.rs,lifetime.rs}`, `collector.rs`, `native-observation/src/uia.rs` (per-call timeout, shared with C1).

**Completion criteria (static).**
- The slot deadline exceeds the request deadline by the cleanup bound.
- The request deadline produces a typed result without stopping the session.

---

### Stage E3. Collector loss continuity, lane pressure, drain and token checks

**Changes.**
- **Scope-serial change.** The raw loop emits the pending gap for the old scope (with its sequence and QPC range) before resetting `Losses`. The same applies in `runtime/input.rs`.
- **Ingress gap crossing the resync sample.** Clip the gap to the part after the sample instead of dropping it.
- **Rejected text-edit commits.** Count them per reason (`check_sender`, deadline, busy, too long, stale, invalid) into the context lane's loss evidence. Sender and foreground mismatches are counted without detail (§5.1). In the Recorder, these commit losses and the collector's TextEdit gaps become input-stream losses with producer `text_edit`, like the commit losses of an ordinary visit (I7).
- **Data-lane writes.** One outstanding overlapped write per lane is kept. Later records queue in the existing bounded outbound queue, and drop-newest with loss accounting applies when it is full (§3.2).
- **Lane failure.** The lane fails the connection only when its single outstanding write has not completed within the essential coverage stall budget (the existing 500 ms `essential_stalled` bound) for the input lane, or the lease bound for the control lane (ALG §6.4: a persistent stall is a source failure).
- **Governed path drain.** Enable the buffered drain (INP-1) with the scope check evaluated once per drained batch, before and after, instead of per message.
- **Token verification.** `verify_capture_target` (token, protection, linked pair) runs on prepare, on each 250 ms renewal and at egress batch boundaries (ELEVATED §4.1: "recheck token facts on prepare and renewal"). Per-message checks use the cached result keyed by grant revision and process incarnation. Process liveness stays per batch.

**Affected modules.** `native-observation/src/input/raw_loop.rs`, `core/capture/elevated/src/runtime/{input.rs,context.rs,owner.rs,grant.rs}`, `core/src/platform/windows/elevated/ingress.rs`.

**Completion criteria (static).**
- (Coordinator addition after the Track B batch 2 review.) Commit losses from the collector's `take_loss` reach the Recorder per reason; the collector does not sum them.
- (Coordinator addition after the I5 review.) The collector forwards the helper-side `OptionalUnavailable` notice; today `eligible()` drops it. A packet the core cannot decode is handled per §6.2: it is counted as a malformed loss and is not fatal to the transport, unless the framing itself is corrupt.
- (Coordinator addition after the I1 review.) An elevated input loss or staging overflow leaves a typed loss or blind marker in the input stream, in addition to the context row `input_source_loss`.
- No `Losses::default()` without a preceding flush.
- No per-message token query.
- A write timeout of 100 ms is no longer connection-fatal.
- `buffered = true` in the governed path.

---

### Stage E4. Elevated closure and evidence completeness

**Changes.**
- **Deferred checkpoints and fixture Saves at Pause/Stop.**
  - Before close, run one poll of the certified prefix.
  - Then resolve every remaining deferred checkpoint as a `checkpoint_context` row with `outcome:"unavailable"` and reason `admission_closed` (or `queue_full` for queue drops). Remaining fixture Saves resolve as `AdmissionClosed`.
  - The control row keeps action, outcome and request ID (C4).
- **Admitted failure.** `cancel()` first attempts `Control::Stop`, then waits for the terminal ack within the cleanup bound shortened by the session deadline, and only then drops the host. `stopped` and `cleanup_failure` are filled from the actual outcome.
- **Lease after Stop.** After Stop is requested, `tick` keeps renewing the lease until `Control::Stop` has been sent.
- **Evidence.** Record the Armed held readiness (`Known`/`Unknown`, no bits), UAC dispatch and return, and withdrawn launches in `ElevatedSourceEvidence`.
- **Return to the standard source.**
  - Require a registration check result from the helper newer than the switch request. The helper's periodic check provides it, and a fresh check is requested over the existing shared-state channel.
  - Every helper environment record attests a registration check run just before it, unthrottled by the periodic check. Its `observed_qpc` is the QPC read before that check, and a record that waits for queue room keeps it. The environment records at stop and at a device inventory are checked the same way.
  - Restart hook-health monitoring with a cleared streak.
  - A failed check keeps the current outcome: `StandardHelperUnavailable`, an admitted source failure (F, §6.2 row 5).
- **Arm QPC.** Taken after hook and raw registration and before the held probe.
- **Bootstrap pipe DACL.** The logon-SID ACE is reduced to the rights the bootstrap server needs (read, write, synchronize), not `FA`.
- **`Secret` moves.** `Secret` values are constructed in place (written into the final location) and passed by reference. Intermediate buffers are wiped. Moves out of `Bootstrap::new`, `receive_frame` and `LaunchArguments::parse` are replaced by in-place initialization.
- **`ticks()`.** Divide by an explicit nanoseconds-per-second constant, with `LEASE` used only as the lease duration.

**Affected modules.** `core/src/platform/windows/input.rs` (`ElevatedSaves`), `elevated/{selector.rs,peer.rs,visit.rs}`, `metadata.rs`, `core/capture/elevated/src/{runtime/grant.rs,pipe/security.rs,wire.rs,secret.rs,runtime/lifetime.rs}`.

**Completion criteria (static).** Each item is present, and no deferred item can be cleared without a terminal outcome.
- (Coordinator addition after the Track A batch 2 review.) No suspend-related deadline can produce F. Today the collector cleanup bound set at suspend (`WorkerSession::cleanup_deadline()` → selector `cleanup_deadline_qpc` → collector) is wall/QPC based and can expire across sleep, which would be a row 5/6 F. It must be evaluated suspend-excluded: either the collector measures awake time with its own power marks, or the Recorder re-issues the bound after resume. A miss caused by sleep is G, not a collector failure.

---

### Stage E5. Target change and ineligible foreground (blocked by D-8)

**Option A.**
- **Recorder-side checks.** In Preparing, Armed, Activating, HeldPending and Refreshing, a failure of `matches_target` or `grant.current()` caused by a foreground or focus change becomes a revoke, not an error. The fatal paths in `visit.rs` are converted accordingly.
- **Before admission.** The selector prepares again for the original candidate while the readiness deadline remains. If the deadline expires, startup fails without admission.
- **After admission.** The epoch stays closed with `ElevationTransition`, and the new foreground is classified fresh.
- **Ineligible approved foreground.** An approved foreground whose `foreground_access` fails with DifferentUser, DifferentSession, Protected or LinkedPair is suppressed with a typed reason, `ineligible_token`, like an unapproved foreground (C5 accounting applies). `fail_classified_source` is used only for failures of the current grant's required access.
- **Remaining fatal paths.** Host failures, transport and authentication failures, lease loss, readiness deadline expiry, witness regression and protocol errors stay fatal.

**Affected modules.** `elevated/{visit.rs,selector.rs}`, `preflight.rs`, `session/windows.rs` (`fail_classified_source` callers).

---

### Stage E6. Collector canonical approval proof (blocked by D-9)

**Option A.**
- **Prepare.** On prepare, the collector opens the image file of the pinned live target process. It obtains the path from the live process handle, opens it with no write or delete sharing, reads the final path by handle and the file ID, and canonicalizes with the same rules as the Recorder's resolver (moved into native-observation so both sides share one implementation).
- **Grant condition.** The grant requires equality with the sealed approval key. Any failure gives `TargetAccess` before admission.
- **Restrictions.** This work runs only on the owner thread, never in callbacks, and never opens a Recorder-supplied path.

**Affected modules.** `core/capture/elevated/src/runtime/{grant.rs,witness.rs}`, `native-observation` (shared canonicalization), `core/src/platform/windows/context/identity.rs` (uses the shared function).

---

### Stage R1. Journal schema conformance and versioning (blocked by D-1)

**Option A.**
- **Input.**
  - `mods` becomes a list of side names (`ctrl_l`, …), with `mods_known` as an extension.
  - `scan` becomes an integer per INP-2, with the prefix in `scan_prefix` (extension).
  - Pointer fields move to top level: `pos`, `pos_recv`, `monitor`, `held` (after the event), `delta`, `coalesced`, `warp`.
  - The current nested details become documented extensions.
  - `input_health` rows: per-second coalesced and thinned counts (replacing `motion_sampling`), registration and hook events.
  - `blind` rows move to `input.jsonl`.
- **Context.**
  - `foreground` with `pid`, `exe`, `rect`, `monitor`, `product`, `version`, `version_string`, `title`, `class`, `approved`.
  - `focus.uia{control:<name>, framework, editable, password}` with `uia:"timeout"`, plus `ime` inside `focus`.
  - `element` with `trigger`, `pos` and `uia{control, name, rect, …}`.
  - Control types serialized by name.
- **Stills.** One row per trigger with `frame_obs_id`, `seen_obs_id`, `file`, `seen_file` (`seen_file` only when it differs, IMG-2). `file` is `images/<digest>.png`, relative to `raw/`. `still_dropped` keeps its reason fields.
- **Frames.** `recv_t_us` follows the §6.5 receipt rule:
  - the true capture receipt for rows that newly select a frame;
  - the ordering receipt max(previous, e_k) for repeats and frameless rows, with `slot_decided_t_us` kept.

  Rows go through the per-monitor lanes and the non-blocking merge of §6.5, with boundary-owned constraints (Opening and Closing) and real-time slot rows. Publication time is kept as `published_t_us` (extension).
  - This part depends on D-10: R1 is implemented after V2b (D-10 = A). The receipt rule itself holds under both options.
- **Frames rows** never drop in an attempt without an F (§6.1 rule 6). The only non-row outcomes are §6.2 rows 19 (L-required, resolved by the worker as rows), 20 (F) and 21 (F).
- **Versioning.** Bump the manifest and package format to v6. Every extension field is listed in the schema document (D1). Reader compatibility follows the D-1 sub-option:
  - **A-v6only:** the reader opens v6 only.
  - **A-v5read:** the reader keeps the current v5 parser as a separate read-only path, and `validate_gates` maps v5 field names onto the same checks.

**Affected modules.** `gate.rs`, `capture/input/{evidence.rs,normalize.rs}`, `context.rs`, `context/queries.rs`, `stills.rs`, `video.rs`, `slots.rs`, `journal.rs` (frames merge), `metadata.rs`, `reader.rs`, the README.

**Completion criteria (static).**
- (Coordinator addition after the T1 review.) The T1 gate checks in the reader are realigned to the v6 field names:
  - gate 3 matches synthetic and ordinary rows by the new key, button and device identity fields, not by serialized identity;
  - gate 4's `*_gap` suffix wildcard is replaced by the explicit list of v6 loss and boundary kinds;
  - the Recorder-side fixed-string `context_source_gap` rows (`recorder_staging`, `optional_staging`) become typed rows, if E3 did not already convert them.
- The serializers produce exactly the fields listed in the schema document, and the reader validates them.
- The frames stream has no loss path except row 21 under an already recorded F.
- Each per-monitor lane plus its worker staging is sized as `staged_slots + ceil(T × effective fps)`. V3 already doubled `staged_slots` to 2 × (segment pictures + margin), so the factor 2 of the original V1 formula is not applied a second time. T is derived from the V3 verification allowance and the other row-21 terms, with no literals. V1 delivered the staged slot bound; the lane term is implemented here, because the lanes are introduced by this stage (coordinator note after the V1 review).
- Frames receipts:
  - rows that newly select a frame carry the true capture receipt;
  - every other frames row carries max(previous, e_k);
  - no code path assigns a frames `recv_t_us` from publication time.
- The controller sends Opening(e, R) to the frames journal owner and receives its acknowledgement before any video worker can observe epoch e.
- The journal owner sets the Open floor on Opening, sets Closed only on a worker's closing mark, and never lets a monitor stop constraining on absence.
- Video workers decide and enqueue every non-paused slot in real time, including during absence and after their own video L fault.
- Only the worker resolves its own staged rows (row 19). The journal owner's mark watchdog (row 21) only reports F and never accesses worker staging.
- The row-21 watchdog:
  - measures T as suspend-excluded elapsed time through the S0 accessor;
  - is not evaluated while a suspend mark has no newer resume mark;
  - applies only while the controller is Recording and the monitor has a current-epoch mark, so a monitor Closing after a pause boundary (including Paused(Sleep)) is never reported under row 21.
- The Opening acknowledgement is sent by the merge-owner thread, which performs no file append or flush.
- The reader's version handling matches the chosen sub-option.

---

### Stage X1. Dead code removal

**Change.** Remove the items listed in §3.6, and the `video: Option` handling in `monitor.rs` (make it non-optional). `CoreToHelper::EnvironmentChanged` and `Stop` are removed from the protocol, because the shared-state atomics are the implemented channel.

**Additional item (coordinator, after the C1 review).** The baseline manifest of `screen-fix-native-observation` does not compile on its own with `--features context`: `events.rs` uses `WNDCLASSW`/`RegisterClassW`, but the crate's `windows-sys` features omit what they need. It compiles in the workspace only through feature unification with other crates. Add the missing feature or features to the crate's own manifest, so that each package compiles with the feature sets it declares.

**Completion criteria (static).** No remaining definition is unused. Grep shows no references to the removed names. `cargo check -p screen-fix-native-observation --features context` passes.

---

### Stage T1. Package gate checks (M1-10, gates 1–4)

**Context.** T-1. The reader validates gate 1 and part of gate 2 only.

**Algorithm.** `PackageReader` gains a read-only `validate_gates` (extending `validate_v5_artifacts`) that streams each journal once.
1. **Gate 1 (journal ordering and parsing).** Every line parses. `seq` is exactly previous+1. `recv_t_us` is non-decreasing. Gap rows are well-formed and reconcile with the summaries (A1).
2. **Gate 2, index part.** The existing segment and index checks. `event_ref` must resolve to an existing input row (the reader keeps a bounded index of input `seq` → kind for rows referenced by stills, built in one pass) whose kind is an action kind.
3. **Gate 2, decode part.** The decode comparison (decoded PTS against `encoded:true` rows within 1 ms, frame for frame) is a separate Windows-only function, `verify_segment_decoding`, in the platform layer. It reuses the verifier of stage V3. The reader stays platform-independent.
4. **Gate 3 (input boundary consistency).**
   - Build the excluded intervals: pause and resume, blind, gap `[first, last]`, discontinuity, and suppression intervals (C5) where applicable.
   - For each `(device, key identity)` and `(device, button)`, scan downs and ups in journal order:
     - a down not marked repeat, and not `repeat:null` (I4), opens a press;
     - the next up closes it;
     - a press open at a boundary must be closed by a synthetic up at that boundary;
     - an up with `press_unavailable:true` needs no press.
   - Report presses that are unmatched outside the excluded intervals, and ups without a press that lack `press_unavailable`.
5. **Gate 4 (pause-stream rules).** Inside each pause interval, from the pause row to the resume row by `recv_t_us` and `seq`, only these kinds are allowed:
   - control rows, pause, resume, discontinuity, blind, gap, and `foreground` with only `approved:false`;
   - no frames rows for paused slots;
   - no stills rows with a trigger time inside the interval.

**Result.** A structured report: per gate pass or fail with the offending stream, seq and reason. It is not called on the desktop or Stop path.

**Affected modules.** `core/src/package/reader.rs`, `core/src/platform/windows/mp4.rs` (verification function exposed for tests).

**Completion criteria (static).** Each gate has a function with the rules above. No check depends on live capture state.

**Verification.** Execution: T2 fixture packages and T4 real attempts.

---

### Stage T2. Algorithm unit tests and fixture packages

**Content.** In-crate `#[cfg(test)]` modules, because the modules under test are crate-private.
- **Journal (A1):**
  - gap emission before later data;
  - boundary placement (no gap rows inside a pause);
  - per-key merge;
  - unknown counts;
  - non-decreasing receipts;
  - reserved capacity under a full queue.
- **HookHealth during a stall (I5):** transitions inside an open `producer_stall` interval are not counted, and the counter is reset when the interval closes.
- **Power marks (S0):** consistent snapshots under a concurrent writer; suspend or resume without a QPC gives an unknown, not a zero, interval.
- **Fault log (A2):** arrival order; first F as the result cause; first F or L as the diagnostic primary cause; identical-cause deduplication; the cap of 32 F reports with the omitted count; L reports always retained; disposition order of rule 4.
- **Matching (I1, I2):**
  - unambiguous key and button pairs;
  - ambiguous key pairs published once with metadata;
  - closing decision up to the cutoff;
  - dense one-device motion at 1000 Hz (every hook sample published, every raw delta attributed once);
  - two-device motion;
  - wheel run pairing;
  - a 2 raw × 2 hook key cluster: exactly 2 rows with `correlation:"ambiguous"`, both hooks `ClusterAbsorbed`, no hook-only row for either hook, and every record in exactly one row or count;
  - a 2 raw × 3 hook cluster: 2 rows and one `cluster_excess_hook` suppression;
  - closure of a component with no ambiguity propagation to an adjacent independent pair.
- **Hook-only attribution (I3):**
  - an approved stable segment with a non-text focus: a hook-only key is published;
  - a password or possibly sensitive visit: the hook-only key goes only into the redacted aggregate;
  - a focus change inside the 16 ms uncertainty interval: Sensitive count, no row;
  - an unapproved segment: `approval_unknown` count.
- **Ledger privacy (A1):** Sensitive entries produce no gap row and no time range.
- **Held (I4):**
  - autorepeat after a boundary gives `repeat:null`;
  - an up without a published down gives `press_unavailable`;
  - modifier resync edges.
- **Slots (V2a, V2b):**
  - integer slot boundaries without drift over 24 h of slots at 30 and 60 fps;
  - watermark decisions;
  - repeat provenance across same-size epochs;
  - PTS mapping (segment-local from the slot start).
- **Fault classification (A2):** each listed condition maps to its class, and the first cause is retained.
- **Fixture packages.** Committed under `core/tests/fixtures/`, with JSONL, a manifest, tiny PNGs and no MP4, plus integration tests in `core/tests/` that call `PackageReader::open` and `validate_gates`. They include one valid package and one failing package per gate rule.

Platform-dependent decode tests are marked ignored by default and run only on Windows under T4.

**Affected modules.** New test modules and fixtures. `[dev-dependencies]` are added only if needed (none are expected; `serde_json` is already a dependency).

**Completion criteria (static).** Each listed case exists, and fixtures cover every gate rule's pass and fail.

**Verification.** Execution: `cargo test -p screen-fix-core` (needs authorization). The workspace default member is `ui`, so the package flag is required.

---

### Stage T3. Trusted fixture Save producer (M1-08)

**Context.** T-2. Routing exists, but nothing produces a confirmed save.

**Design.**
- **Producer.** A non-default Cargo feature `fixture-save` on the desktop crate adds a developer-only "Confirm fixture save" control.
  - It is visible only while Recording.
  - It calls the existing `SessionHandle::confirm_fixture_save` with the currently approved executable identity and the selected monitors. Leaving the monitors empty uses the existing window-geometry routing.
  - It shows the returned `FixtureSaveResult`.
  - The producer is trusted because it is compiled into the Recorder itself. No external IPC or file input can create a confirmed save. Release builds without the feature carry no producer.
- **Final saves.** A second action, "Confirm final fixture save", sets the final-save kind, so `final_save_settled` is reachable.
- **Automated harness.** A Windows-only integration test (ignored by default, because it needs an interactive desktop and execution authorization):
  1. starts a recording through `platform::start_recording_with_hotkeys` with a test configuration approving a known executable (for example the test's own child window process);
  2. waits for Recording;
  3. calls `confirm_fixture_save`;
  4. stops;
  5. checks with `validate_gates` that `stills.jsonl` contains `checkpoint` (and `final_save_settled` for the final variant) rows that reference the save.
- **Documentation.** Document the feature in the README (D1).

**Affected modules.** `ui/Cargo.toml` (feature), `ui/src/app.rs` (feature-gated control), `core/tests/` (harness).

**Completion criteria (static).**
- The feature is off by default.
- The control calls only the public seam.
- No other producer path exists.

---

### Stage T4. Execution verification campaign (needs user authorization)

**Steps, in order, each reported separately (A1, M1-10):**
1. `cargo build --workspace`, plus `cargo clippy` if authorized.
2. `cargo test -p screen-fix-core` (T2 and T1 fixtures).
3. Windows 10 and Windows 11 partial scenarios on the available hardware:
   - **T1 (shortened):** gates 1–4 via `validate_gates` and decode verification.
   - **T3:** scaling change, unplugging a monitor.
   - **T7:** drag with a held Shift, 60 s.
   - **T11:** layouts, IME, a password field, injected Unicode.
   - **T22:** click, drag, shortcut burst, hover, render, forced still burst, and fixture save via T3.
   - **T23:** flood the input queue, kill the helper, low disk, HDR switch.
   - **Pause, resume, sleep and lock.**
   - **Elevated handoff** (both directions), where the signed installation is available.
4. Record OS builds, GPUs, resolutions and pen model (A1). Mark T8b and the full T20 as pending.

Results go to a separate evidence file. A source review or a successful build is not acceptance (spec addendum §6).

---

### Stage D1. Documentation updates

**Changes (all documentation in English):**
- **README:**
  - Correct the `Cargo.lock` statement: it is tracked.
  - Correct the description of `validate_v5_artifacts` (or `validate_gates`): which gates it covers, and that gate 2 decode is a separate Windows check.
  - Document the `fixture-save` feature and the test command (`cargo test -p screen-fix-core`).
  - Update the journal field descriptions after R1.
  - Describe the gap row fields and the `health` block.
- **New `doc/SCREEN_FIX_M1_JOURNAL_SCHEMA.md` (after D-1).** The versioned schema per stream, listing PDF fields and extensions, with the package format version.
  - **Frames.** It states explicitly that for repeated slots and slots without a frame, `recv_t_us` is an **ordering receipt** (§6.5), not an observation time. It exists only to keep the stream's receipt order. Timing analysis must use `slot_decided_t_us` (slot decision), `source_t_us` (frame capture) and `capture_recv_t_us` (frame receipt), never `recv_t_us` of such rows.
  - **Frames.** It states that `recv_t_us` is the true capture receipt only on rows that newly select a frame.
  - **Frames.** It states that, by design, a repeat or frameless row's `recv_t_us` can be less than its `slot_decided_t_us`, because the ordering receipt is the slot end e_k bounded below by the previous row. Consumers must not treat that difference as an anomaly.
- **`doc/SCREEN_FIX_M1_SPEC_ADDENDUM.md`.**
  - Record the accepted amendments from D-2, D-3, D-5 and D-6 as explicit amendments.
  - Add a clarification that `checksums.sha256` (DATA-3) and gate 8 start with M2 per PDF §17.
  - Record a deliberate M1 limitation under A2: a capture-thread failure that is not a topology change (for example, GPU device loss, §6.2 row 4) fails the attempt (F). The VID-9 device-loss rebuild is deferred together with the other recovery work, so the PDF requirement that "none ends the attempt" is not met for device loss in M1.
- **`doc/SCREEN_FIX_M1_ALGORITHMS_ADDENDUM.md`:**
  - The loss ledger and terminal outcomes (§6.1).
  - Fault classes (§6.2).
  - Closing certification (§6.3).
  - Hook-only foreground attribution (§6.4).
  - The slot watermark (V2b), recorded as an explicit amendment of the ALG §9.1 decision point after D-10, with the VID-4 and ALG §9.1 source references.
  - The focus-classification rules chosen in D-4 (sub-questions a–d), recorded as an explicit clarification of CTX-9 against ALG §7.1.
  - Segment verification placement (V3).
  - The motion association rule (after D-7).
  - Boundary key state (I4).
  - Fault attribution as implemented in A2 (coordinator addition after the A2 review):
    - the owning component at each boundary (COM/MF initialization and monitor preflight: video; topology, enumeration and OS capability: capture; approved-application preflight: context; hotkeys, disk, environment and clock: controller; indicator and window tracking: interface; input worker: receiver; helper errors: input helper);
    - the result message of a failed attempt names the first F;
    - the primary cause recorded in diagnostics is the first F or L, with the remaining reports attached as secondary causes.
  - C1 behaviour changes (coordinator addition after the C1 review):
    - Value read-only with Text writable: `editable:true`, per the CTX-9 "either … is false" rule.
    - Value read-only with Text undetermined: `editable:null`, now redacted.
    - Value pattern present but its attribute unavailable, with Text read-only or absent: `editable:null`, now redacted.
    - The `hwndActive` fallback serves keyboard layout and focus identity only, never password evidence.
  - I5: a helper failure noticed after the pause prefix is published writes no runtime input health row; the final activity-loss row remains. Per-second `input_health` counts exclude the privacy-adjacent suppression classes (approval unknown, not approved, control uncertain, cluster excess).
  - Track B batch 1:
    - Any type change in `protocol/src/ipc/elevated` requires a bump of `attachment::VERSION`.
    - I4: a down without a published press, and its later autorepeats, are published with `repeat:null` and `held_across_boundary`, and the final up with `press_unavailable`. The hold does not enter the published held state.
    - I2: a raw packet received within +W after hook sample h is attributed to h, so in dense motion a delta can lag by one sample.
  - Track A batch 1:
    - V3: a rejected segment verification is L (row 9). Only an unfinished required segment at the Stop deadline is F.
    - V3: an already published `.mp4` is never deleted on a later elevated revocation; authorization is checked right before rename. The remaining race window is benign, because the segment contains only frames admitted while the visit was approved.
    - V4: encoder selection failure at reconfigure is L-required for that monitor (capability selection, not recovery).
  - Known deviation: workers still mutate the shared `Facts` through `WorkerSession` (ARCH §4). The full owner/message refactor of S4 is deferred.
    D1 lists the mutating `WorkerSession` methods. The state-machine description matches the code (several latch flags, not one enum).
  - S1: no separate hotkey-registration-loss health. A `RegisterHotKey` registration is bound to its thread and is lost only when that thread stops, which is already detected and shown.
  - S3: an admitted interval of a present monitor counts as "without video" only if it contains a slot decided later than start + G and has no encoded slot. Shorter intervals are recorded with a short-interval flag and do not affect Complete. ApprovalSuppressed slots lie outside admitted intervals.
  - P4: "complete" packages recorded before the inventory change (no `images`/`cursors` lists) fail validation. This is a deliberate incompatibility, consistent with D-1 A-v6only.
  - E3: token facts are verified on prepare, renewal, focus refresh, activation and once per egress batch and grant. The window, thread and desktop binding and the foreground tuple are still checked per record. Deliberate trade-off: if the token changes mid-batch, the rest of that batch can still leave; the window is bounded by the batch size.
  - §6.2 row 6: the decision is made at request deadline + 750 ms, and termination follows after the cleanup bound (worst case about request deadline + 1.5 s).
  - The Stop finalization reserve (I1: `STOP_FINALIZATION_RESERVE` = 2 s), with its rationale: under the 10 s Stop deadline about 8 s remain for certification, and under the 3.75 s session-end budget about 1.75 s.
    - Invariant for every consumer of `FocusAtReceipt.password_style`: `None` never means "not a password". Only `Some(false)` is password-safe, so no consumer may test `!= Some(true)` without also excluding the fallback. State this in the doc comment of `FocusAtReceipt` as well.
- **`doc/SCREEN_FIX_ELEVATED_COMPONENT_SPEC_ADDENDUM.md`:**
  - Clarify that the payloads are typed serde values inside the binary envelope and that "JSON rows" means journal rows.
  - The readiness budget per preparation.
  - The D-8 and D-9 outcomes.
  - Provider request and slot deadlines (E2).
- **`doc/SCREEN_FIX_M1_ARCHITECTURE_ADDENDUM.md`.** Document the permitted process-wide statics (one-recording latch, report serial, callback context) as the explicit exceptions to "no global mutable state".

**Completion criteria (static).**
- Every behaviour change from the stages above is described.
- No document claims a check or test result that was not executed.

## 8. Verification summary per stage

| Stage | Static review confirms | Requires compilation or execution |
| --- | --- | --- |
| A1, A2 | APIs, emission order, classification table, no null receipts | Compile; T2 journal and fault tests |
| I1–I8 | Removal of correlation rejection and fatal per-packet paths; accounting at each discard | Compile; T2 matcher and held tests; T4 T7, T11, T23 |
| C1–C5 | `editable` rules, accounting, callback filter, no Err for identity | Compile; T4 T11, password fields, Ghost window |
| V1–V5 | Derived capacities, decision rule, verifier placement, no suspend failure | Compile; T2 slot tests; T4 60 FPS, T1, T3, sleep |
| S0–S4 | Suspend marks and callback hygiene (S0), no latches, UI branches, single controller-owned state | Compile; T4 lifecycle scenarios |
| P1–P4 | Reasons, cancellation, shared work, inventory | Compile; T4 T22 |
| E1–E6 | No discard while pending, deadline margins, loss flush, closure | Compile; elevated scenarios on the signed installation |
| R1, X1, D1 | Field lists against the schema document; no dead definitions | Compile; T1 on real packages |
| T1–T3 | Gate rules, fixtures, feature gating | `cargo test -p screen-fix-core`; Windows harness |
| T4 | — | Entirely execution |

## 9. What could not be verified statically

- How often V-2 (late frames), I-1 (dense-motion ambiguity), I-2 (closing loss) and EarlyRows exhaustion occur. The code paths are confirmed, but the magnitudes need T4 runs.
- Whether Raw Input delivers SendInput packets with `hDevice = NULL` on every supported build. This affects how I3's injected pairing behaves in practice.
- Whether UIA providers that do not explicitly support `IsPassword` return a value through `GetCachedPropertyValueEx(ignoreDefault=true)`. If not, many fields are over-redacted. C1 does not change the password rule. T4 T11 should check.
- Whether an ordinary Stop produces a non-empty `uncertified_closing_tail` today. Under §6.3, a non-empty Stop tail remains an input-certification shortfall (`input_unavailable`, as in the current code and C6). Stage I1 removes the horizon and extends the Stop wait so that ordinary Stops produce no tail. T4 should confirm the disposition of a normal Stop.
- Whether `Instant`-based deadlines advance through sleep on the target systems. V5 and S1 switch to suspend-excluded QPC intervals regardless.
- Whether a runtime Raw Input registration mismatch for the optional collections actually occurs (I5 removes its fatality regardless).
