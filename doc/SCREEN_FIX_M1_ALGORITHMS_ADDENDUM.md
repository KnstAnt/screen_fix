# M1 algorithms addendum

Date: 2026-11-15 (Europe/Moscow)
Project: screen_fix / Intercall Recorder
## 1. Scope and authority

Read with SCREEN_FIX_M1_SPEC_ADDENDUM.md and Intercall Recorder technical spec v1 Windows.pdf. The accepted scope addendum takes precedence where it records an amendment, including the ELC-1–6 exception. The [elevated capture specification addendum](SCREEN_FIX_ELEVATED_COMPONENT_SPEC_ADDENDUM.md) records its authorization and source lifecycle. This document resolves implementation mechanics within that scope; it does not replace the PDF schemas or remove other requirements. target.txt and the PDF remain unchanged.

M0 is complete. M1 implements the capture core. Automatic fault recovery, production application adapters, full privacy/review, derived actions and upload are outside this design. The M1 allow-list is a test configuration, not a production privacy claim.

Constants identified as initial engineering settings below are bounded implementation choices, not measured device characteristics. Record the effective settings. Hardware validation may change these settings without changing the algorithm's meaning.

## 2. Owners, values and execution

| Owner | Exclusive mutable state and resources | Input/output |
| --- | --- | --- |
| Session controller | Recording state, pause reasons, session identity, finalization status | Typed UI/control requests; status and bounded stop requests |
| Standard helper and elevated collector | Shared native input producers, device descriptors and producer-local progress. The elevated collector also owns its exact local witness and scoped native reads. | Timestamped typed evidence, readiness, loss and progress |
| Input Receiver and gate in core | Matching, private and published held state, classification and bounded ordered input staging | Selected native source and context evidence; publishable input and observation decisions |
| Context service | Window/display snapshots and context ordering | Windows notifications; context values and UIA requests |
| UIA worker | COM/UIA objects and subscriptions | Requests with IDs/generations; value-only results |
| Per-monitor capture owner | WGC, immediate GPU context, native textures, frame history, thumbnail and staging resources | Wake signals and observation requests; immutable frame metadata/readback results |
| Per-monitor video worker | Encoder, muxer, bounded video queue, segment state | Owned samples; segment results and frame outcomes |
| PNG worker | One CPU pixel job at a time, hash/file creation | Packed pixels; image hash and success/error |
| Journal/output owner | Append-only journal files, sequence allocation and pending publication slots | Finalized record values; flush completion |
| Diagnostic writer | Its report file | Bounded error/context snapshot; report path or write error |

These are ownership roles, not a requirement for a separate thread per row. Session control can share the UI event loop because it performs no blocking IO. Input gating and journal ordering may share a core processing thread, but file writes must not block input collection, capture or the UI. Use a dedicated writer where necessary.

Hooks and WinEvent callbacks only copy bounded evidence into preallocated queues and return. WGC callbacks only signal. Do not perform matching, UIA, compression or file work in callbacks. Rust FFI boundaries contain unwinding as prescribed by ARCH-2; unsafe code stays in narrowly documented Windows wrappers.

Use small enums/newtypes for IDs, times, dispositions and errors. A request receives a value result, never a callback into another owner's internal mutable state. No generic executor or scheduler is introduced.

A texture remains mutable only to its capture owner. While retained by an encoder/readback consumer it is not overwritten. The consumer owns an immutable resource lease or transferred sample until completion; releasing that lease returns eligibility for reuse. Encoder input acceptance is not assumed to mean that the encoder has finished reading the resource.

## 3. Time, stream order and references

### 3.1 Timestamp rules

- Use one QPC origin/frequency across core and helper. Convert with i128 intermediates, subtract before scaling, and check narrowing.
- Store provider source time only when available. Raw Input/hook/WinEvent coarse ticks never become an exact source_t_us.
- WGC source time is mapped using CLK-3, not the video PTS.
- Preserve original receipt timestamps through matching and asynchronous work. A later completion has its own receipt time; never overwrite the original event time with completion time.
- UTC anchors are metadata; UTC changes never reorder session events.
- Record stream-local seq as the tie breaker. Equal times across streams do not prove causality.
- Use explicit observation IDs and event references; never associate records merely by equal timestamps.

### 3.2 Ordered publication with bounded work

Each stream has a small owner-controlled staging sequence. Reserve a private slot when its logical record is received, before matching, motion sampling, hotkey recognition or UIA can delay it; mark it Ready, Suppressed, Dropped or Pending. Assign serialized seq only to actual output rows in receipt order, without persisted placeholders for suppressed slots. Only a contiguous resolved prefix is eligible for append. Requests use private IDs until their published input seq is known; resolve event_ref before appending the dependent observation.

Input classification may hold that stream's prefix while UIA is pending. Still encoding may hold the still stream's prefix. Neither stalls capture nor another journal. Queue sizes remain bounded; expiration/drop resolves a slot instead of waiting forever. A suppressed sensitive key produces no per-key persisted placeholder.

Frame rows wait for the corresponding encoder outcome and are published in slot order. Still rows are published only after image persistence succeeds; otherwise resolve to a still_dropped/health outcome. An image filename must never promise a nonexistent successful file.

A stream with several event producers uses a local merge, not a global merger. Each producer publishes records in receipt order and a progress marker only after all earlier records have been enqueued or explicitly resolved as missing. At every processing stage this marker must also stay at or below the earliest unresolved receipt time, including matching, retained last-motion samples and hotkey prefixes. The stream owner releases only records strictly before the minimum of those bounds. Progress markers are internal and carry no personal payload. Idle message loops publish progress on their existing timed wake. If progress fails beyond the configured bounded wait, record the affected blind/gap interval and diagnose the producer; do not silently reorder late data or create replacement workers. Local deadlines resolve withheld slots before advancing the marker; metadata from a late result can never resurrect a closed slot.

Late UIA/PNG/encoder results identify an existing request slot. They do not insert an old-timestamp record into an already written prefix. Context request-completion records use completion receipt time and carry separate request start/end times. Completed redacted visits use completion receipt time with a start/end range, so a long password visit does not hold the journal open indefinitely.

The exact serialized field names follow the PDF; any metadata extensions are versioned and documented in the implementation, rather than silently changing the meaning of an existing timestamp.

For collector-backed visits, the ingress also checks authenticated producer sequence/position, exact visit and exclusive context coverage. The Receiver caps materialization at the minimum of native bounds, unresolved matching/control/health work, context proof and the closing cut. Reserved notices constrain coverage from their conservative native source time. A context completion keeps the child's completion QPC and the Recorder's successful read receipt separately. Optional results materialize in receipt order within certified context coverage or expire. Essential notices and unrelated journals continue independently. A late completion that would precede an already written prefix is resolved through the existing unavailable or ordered loss path.

## 4. Session algorithm

States: Idle, Starting, Recording, Paused, Stopping, Finished, Failed. Pause reasons are a set, not a single overwritten value.

| Transition | Action |
| --- | --- |
| Idle -> Starting | Freeze configuration; allocate session/output identity; create incomplete manifest; initialize workers; check selected encoders/input capabilities |
| Starting -> Recording | All required startup checks succeeded; establish recording boundary; synchronize known input state; arm start observation per available monitor |
| Starting -> Failed | Report specific initialization failure; release successfully initialized resources; no healthy-session claim |
| Recording -> Paused | Close admission at boundary; close published held input; finish admitted work and current segments; publish pause/flush |
| Paused -> Recording | All automatic reasons cleared and user/low-disk resume conditions satisfied; same clock, new segments, state synchronization and start observations |
| Recording/Paused -> Stopping | Establish Stop boundary; cancel future triggers; bounded completion of admitted work |
| Stopping -> Finished | Required finalization/flush succeeded; manifest published last |
| Any active state -> Failed | Record explicit failure; stop affected work and bound ordinary cleanup; preserve files; no automatic restart |

Repeated commands are idempotent for their state: repeated Stop does not close files twice, repeated Pause does not emit duplicate synthetic releases. Resume cannot override an active lock, secure desktop or low-disk reason. Clearing lock does not clear a user pause.

Events before a control boundary that were already admitted remain eligible. Events at/after the boundary are rejected for normal input/still/video publication until resume. Internal boundary messages acknowledge the drained producer prefix only where needed to order pause/Stop; unrelated processing continues.

For manual M1 boundaries use the controller receipt time. When a coarse Windows boundary is available retain its uncertainty. Full retroactive privacy hold/cut processing belongs to M3; do not claim that M1 removes already persisted content preceding detection of an external switch.

Before pause/Stop, release only keys/buttons known to the published stream, with synthetic=true and a boundary reason. Keep physical/private helper state separate from published state. No ordinary input rows are written during pause; permitted control/boundary rows follow the PDF. Accumulate suppressed health counters in memory and report them after resume or in final metadata, without adding prohibited paused-stream activity.

At resume/start, synchronize modifiers/buttons only when desktop and integrity checks make the reads meaningful. Unknown is not released. An ordinary key whose earlier down was hidden by a gap/pause is not given a fabricated physical down; retain a marked boundary/unknown-state context so subsequent up is not treated as an ordinary complete pair.

For foreground source selection, protected preparation and handoff:

1. Classify the actual approved foreground at countdown completion or during recording. Require an exact process incarnation, foreground window/thread, interactive desktop, session, access and approval proof. Suppress stale or uncertain observations. An elevated Recorder uses the standard source when the non-SYSTEM target satisfies the shared interactive user and session checks, has integrity at or below the Recorder’s actual integrity, and passes the same-sign-in or reciprocal linked-token check. This direct-access decision also feeds the standard context and point checks. An inaccessible elevated hit away from the foreground cannot launch the collector.
2. If a separate collector is required and the actual Recorder and target form the supported medium-to-high split pair, clear frame approval and close any open standard-source epoch before scheduling Windows consent. A one-flight launch retains the original consent and authentication deadlines. When the selector detects a transition to the collector from an open standard-source epoch, clear frame eligibility immediately and latch the source change against that epoch. The old epoch stays closed to new standard-source work if the foreground returns before the controller processes Pause. An independent pause retires the old authorization and cancels an undispatched request. A dispatched consent dialog can finish while capture stays closed. Stop cancels the ticket even if Windows still shows the dialog. A stale shell result or late readiness reply cannot admit work. Reuse a healthy authenticated attached collector for a freshly authorized grant when possible. Startup consent remains tied to the original foreground candidate. Once the host is configured and idle, a newer live preparation independently selects and approves its actual foreground.
3. Both sides verify the exact live target after consent. Prepare the grant closed, confirm required producer registration and the armed held-state probe, and establish the ordinary Start or Resume boundary only after independent pause reasons clear. Activate the exact admission epoch and verify it on the high side.
4. Sample held state again after activation on the selected source. Bind the actual sample, request, grant, access proof and QPC to that boundary. Required authority or access failure closes capture. A valid typed unknown supplies no released bits or synthetic physical edges. The initial collector coverage stays suppressed or uncertified through the fresh resync and eligible progress; the controller retains the actual Start/Resume time.
5. On a switch between the standard source and the collector, or on collector-backed target replacement or invalidation, revoke the old scope, bound the retired prefix, discard cross-visit state and reopen on fresh source readiness and resync. Inactive standard records are drained without feeding the Receiver. An idle collector has no active target producers or subscriptions. Lock, sleep, unavailable desktop, manual pause and low disk retain independent reasons. Unknown or unapproved foreground stays suppressed; a confirmed required source fault takes the failure path.

## 5. Capture and frame retention

### 5.1 Per-monitor capture loop

1. Wake on frame availability, pacing/readback timer, configuration/control request.
2. Read available WGC frames. Check ContentSize and geometry version. Close each borrowed frame after copying; never retain its surface across pool recreation.
3. For a valid frame, obtain an available recorder-owned native texture. Apply fixed HDR-to-SDR conversion when required; history and PNG pixels are native-size SDR BGRA8.
4. Attach monotonically assigned frame ID, source/receipt time, geometry version, dimensions and color contract.
5. Produce the 256x144 luma thumbnail. Score is mean absolute luma difference normalized to [0,1]; 0.002 refers to this normalized scale.
6. Install the frame into the four-entry paced history and run pending temporal selectors before releasing displaced history references.
7. Submit eligible video-slot work and nonblocking staging-copy work.
8. Poll staging readiness without waiting. Return quickly to collection.

Capture pacing follows the selected capture profile (normally 30 FPS; supported 60 FPS profiles retain their PDF qualification). Encoding may consume a subset at 15 FPS. A repeated video frame does not create a new captured frame or satisfy a first-new-frame request.

No-image availability is a normal wait, not a capture error. A healthy unchanged desktop may deliver no new WGC frames. Do not repeatedly copy an unchanged image solely to create new capture timestamps.

### 5.2 Retention accounting

Four history references and one settled-candidate reference are the baseline. Additional pending requests may retain older frames, but count unique retained image bytes, not reference count. Maintain independent hard bounds for retained bytes and number of jobs.

Initial pending-still targets remain IMG-3: 256 MB per monitor (decimal bytes, 256,000,000) and 256 jobs. Count pinned pending images as well as CPU pixel buffers conservatively; do not use the CPU queue limit to justify unlimited GPU pinning. Temporary readback duplication counts until one representation is released.

The pool may grow only to its explicit configured limit. Do not allocate one full-resolution copy per logical trigger. When an image is already retained, add a request reference. Do not overwrite an image while any lease remains.

On pressure, drop eligible queued optional work in IMG-7 order. In-flight work already using staging/encoder resources is not recycled until its owner releases it. If an important request still cannot be retained, report that request as dropped; importance is not a promise of infinite memory.

Maintain independent bounded encoder-sample capacity; when unavailable, mark the affected video slot encoded=false and keep input/still processing alive. Repeated pressure is visible in health counters.

## 6. Input collection, matching and state

### 6.1 Collection and matching

Raw Input and hook evidence are timestamped at collection and retain producer sequence. Raw Input processing follows INP-1, including buffered drain; each HID report is length-checked. Callback queues never grow dynamically.

Normalize matching fields before comparison (scan/E0/E1, transition, logical swapped button, wheel direction/value), keeping original fields. Split compound raw mouse packets into logical transitions/movement as necessary, all retaining their packet identity.

Initial matching wait: at most 20 ms of receipt-time separation, recorded as an engineering setting, not device calibration. Exact suitable bounds are later measured. The raw record's receipt time remains unchanged by this wait.

For key/button/wheel transitions:
1. Compare compatible pending candidates within the window and the same continuity interval.
2. Match only an unambiguous order-consistent pair; consume each source record once.
3. Multiple indistinguishable candidates remain ambiguous; do not choose merely the nearest timestamp and claim certainty.
4. At expiry, emit unmatched evidence with null unavailable fields and the specified reason/origin rules.

Motion is not assumed to be one raw packet per hook. Preserve hook samples during held motion; group unambiguously associated raw deltas into a sample with a coalesced count. If coalescing/device ambiguity prevents association, preserve the raw evidence and hook evidence as unpaired observations, without copying the same delta onto several samples. A raw-only observation is not counted as an additional confirmed cursor move. Keep sampling and matching counts separate.

Unmatched injected hooks and VK_PACKET can be legitimate input, not automatically duplicate/lost records. Origin classification follows INP-7 precedence: promotion signature, injection flags, then qualified device evidence. Unknown remains unknown. Missing calibration leaves warp classification unknown rather than inventing a threshold.

Detect helper/hook/registration failure and record diagnostics/blind evidence. Automatic hook reinstallation, registration repair after loss and helper restart are not implemented as hidden M1 recovery. Initial registration and normal arrival/removal handling remain required.

### 6.2 Sampling and foreground evidence

Keep all key/button transitions except PDF control/password exclusions. Keep all motion samples while a button is held or a configured stroke is active. Otherwise emit the first sample immediately, retain the latest sample in each 1/60 s sampling window, and flush the last pending sample on idle, transition or boundary. Report thinning/coalescing counts.

Foreground approval must be established for the event's receipt context, not from a later UIA result. Capture a lightweight foreground identity with normal input receipt processing; hook callbacks remain copy-only. If a hook-only event's receipt-time foreground cannot be established from available ordered context evidence, conservatively suppress it as approval-unknown rather than attribute it to a later foreground. Do not log unapproved window details.

Pointer events retain their own hook position where matched, separate pos_recv, monitor and geometry identity. An unselected monitor produces no still or pointer UIA query. Ambiguous monitor attribution must not default to the primary display.

Elevated low-level hooks copy bounded source evidence and a cheap foreground tuple. The copy gates require an active grant with an unchanged scope serial and witness revision, fresh witness progress and current attachment and grant QPC leases. The collector owner compares the tuple and source timing with the pinned live process and witness. Native detailed reads and egress check the grant before and after work. A changed revision, uncertain source interval, expired lease or stale grant suppresses payload. The Recorder independently verifies its live scope and certified coverage before materialization. Scope loss can precede delivery of its notification; use the conservative lower QPC or the last proven prefix when needed. Device identities are translated through a bounded source-instance/native-lifetime inventory.

### 6.3 Private and published held state

Maintain:
- Physical/private state used for repeats, normalization and resynchronization.
- Published held state used to form valid journal boundaries.
- Control-chord consumption state.
- Redacted-visit state that never emits key identities.

Update physical state from evidence even when a record is excluded from output. A key release changes the corresponding state once. Auto-repeat does not create another held instance. Device disappearance/gaps invalidate the relevant knowledge.

Register and validate hotkeys as in INP-9. Buffer a possible recorder-only modifier prefix until it is resolved by a chord or ordinary application use; bound the buffer, and resolve overflow with explicit uncertainty rather than infinite retention. A confirmed recorder chord is a control event and its exclusively control-owned transitions/releases are suppressed. A modifier already published for an earlier application interaction is not retroactively deleted: close its published holding synthetically at the control boundary if needed, and consume only the new control-owned transitions. Never leave an unmatched published down or swallow a later unrelated key.

M1 checkpoint control is recorded as a request; it is not a confirmed save. Only a confirmation from the narrow checkpoint input (test producer in M1) triggers checkpoint observations.

### 6.4 Gaps

On queue overflow, drop newest records as REL-2 requires. Keep counts and first/last missing times outside that queue. Emit the gap marker before the first retained post-gap data once capacity returns. Marker receipt time is its publication/admission time; its range preserves the earlier loss times.

Close or invalidate known held interactions at the gap boundary and clear pending matches. When desktop checks permit, synchronize modifiers/buttons with marked synthetic changes. Do not infer missing ordinary key presses, motion paths or pen pressure.

On the collector path, explicit producer gaps retain their conservative missing bounds and require fresh resync. Confirmed required hook loss closes the exact visit at the first missed QPC in the confirming streak. Further unpublished work cannot extend that prefix. A persistent required-path coverage stall is a source failure.

## 7. Focus, UIA, text and pen

### 7.1 Focus generation and bounded classification

On focus/layout/approval change, increment focus generation and immediately invalidate old text evidence. Enqueue a focus request carrying generation, HWND/process identity and request time. New generation supersedes queued obsolete requests. One active UIA call is allowed; no new worker is spawned to evade a stuck call.

Use the PDF UIA timeouts. Per input batch, classification waiting is bounded by a 500 ms engineering deadline from first unresolved key; a queue wait is included, not followed by another unbounded call wait. A later result cannot retroactively reveal a batch already conservatively redacted.

On result:
- Wrong generation/window or stale evidence: do not apply to current keys.
- Password or unresolved possibly editable field: aggregate one redacted visit; discard key/text payload from output candidates.
- Known non-password editable field: apply text-entry rules.
- Known non-editable field: preserve keys as non-typing.
- Conflicting non-sensitive evidence: typing unknown; do not guess a shortcut.

A failed call resolves waiting classification conservatively. If the API call itself remains blocked beyond its expected bound, diagnose the worker; UI/capture remain responsive and no replacement worker is created in M1.

While classification waits, raw collection and bounded private state updates continue. Resolve the input journal prefix on result/deadline. Never dump buffered sensitive events into diagnostics.

### 7.2 Text and delayed evidence

Use INP-3 layout/dead-key/AltGr processing and explicit IME state. Do not label a layout-derived character as proof that the application committed it. Under IME, use finalized composition evidence or unknown; never generate independent text from composing keys as though committed.

Keep confirmed/inferred origin as metadata under a versioned schema if the baseline lacks that distinction. A text record is its own event; Ctrl+S and viewport keys are not text solely because they map to characters.

Password visits produce one aggregate with start/end and count, no per-key timing/identity. On focus end/pause/Stop resolve the visit once. Unresolved buffered keys in a potentially sensitive field are counted into that aggregate. Old evidence cannot reopen a closed visit.

Snapshot reservations for unresolved keys remain private. If classified as password/redacted, release per-key reservations without emitting per-key observation/drop records that would leak timing. Aggregated health may count suppressed work without identifying keys. Full visual blackout remains M3.

The Recorder’s Context owner selects standard native workers or collector reads scoped to a grant. Each collector request identifies the expected process/window, active visit and revisions, original request ID and absolute deadline. Focus and ancestor extraction use approved membership and native password precedence. Focus revisions invalidate subscriptions, scheduled IME, per-key IME and confirmed commits. The collector text-edit callback performs sender proof under its provider deadline and rechecks the exact visit and edit revision at its copy gate using the same live grant, witness and lease checks. The independent supervisor still owns timeout and teardown. Keep key staging deadlines and original callback/source times. For opt-in clipboard, the high collector supplies typed editable-focus fences only. The Recorder’s clipboard owner checks notification age, owner, format and sequence, and publishes only after an eligible post-read fence on the same visit.

### 7.3 Pointer element and pen

UIA pointer queries use physical point, geometry version, approved-window identity and start/end times. A cached result is reused only for the same valid context/point and the PDF age bound. Queries started after the click stay post_action. Do not relabel them as pre-action targets. Hung applications are not queried.

Pen collection parses every validated HID report by report ID/link collection and sub_index. Missing fields are absent, null states are null, signed values/ranges follow INP-11. Retain all contact/stroke samples. Source Scan Time mapping starts a new base after the PDF discontinuity condition; absent scan time remains null.

Link promoted pointer events only when stroke/time evidence is unambiguous. Pen and mouse observations retain separate provenance; do not count a linked promotion as a second physical press or discard pen measurements. No Wintab context is introduced.

Pointer requests preserve the original physical hit and root in addition to foreground scope and geometry. Approved hits within the Recorder’s access scope use the local backend. A point hit or separately observed window that requires the collector needs its own exact approved claim within the active grant. Inaccessible or changed targets produce unavailable evidence. Pen evidence follows the same selected input visit and existing decoder, without cross-source matching.

## 8. Observation algorithm

### 8.1 Trigger lifecycle

A trigger has a private request ID, monitor, geometry/continuity interval, event reference when publishable, role and timestamp. Its states are PendingSelection -> Retained -> Readback -> Encoding -> Saved, or Suppressed/Dropped/Failed.

Temporal selection happens before slow classification/encoding. A provisional pre_action/pre_motion candidate MUST request retention immediately, before waiting for matching or UIA, but no PNG/journal row is published until approval/text/control classification permits it. If matching changes the applicable device calibration, reselect from retained history where possible; otherwise mark the result uncertain. Suppression releases retention.

If a delayed request arrives after its eligible frame left bounded history, return unavailable/uncertain; do not select a later frame. Matching/UIA delay is not a reason to fabricate the before image.

Input-trigger monitor routing: pointer actions use the event's selected monitor; keyboard actions use the approved focused window's monitor from the event's context. If the relevant monitor is unknown, record unavailable rather than capturing an arbitrary screen. Global start/final/idle observation operate independently on each selected available monitor. A confirmed-save signal identifies its affected monitor(s); absent application geometry, use selected monitors containing the approved window and record that routing basis.

### 8.2 Temporal selectors

For input time t and measured applicable delay d:
- adjusted = t - d.
- before = newest frame with source time strictly less than adjusted.
- seen = newest frame with source time <= adjusted - refresh_interval.
- Without valid calibration, use receipt time only as a provisional boundary, mark calibration missing/uncertain; do not imply d=0 was measured.
- Without known refresh interval, seen is unavailable/uncertain.
- Choose within the same geometry/recording interval. Do not bridge pauses or monitor removal.
- A later eligible frame must not replace a missing earlier one.

Store logical before and seen references even when their image bytes deduplicate to one file. Missing seen must not cause a valid before to be discarded.

For next after end time e, first search retained history for the earliest distinct captured frame whose source time is > e; only if no such frame has arrived yet, arm a future selector. Check the frame sequence/eviction boundary: if intervening eligible frames may already have been evicted, the first-frame result is unavailable/uncertain, not a later frame asserted as first. It need not have different pixels. A duplicate image still supplies a distinct observation time. No new frame is unavailable/pending, not a fabricated refreshed capture. Provisional release/timer evidence should arm this selection before delayed normalization completes, subject to the same suppression rules.

### 8.3 Trigger rules and overlapping actions

| Trigger | Algorithm |
| --- | --- |
| start | Arm per monitor at start/resume; select first valid frame after boundary |
| pre_motion | On first movement after >=300 ms without movement, reserve before/seen |
| pointer_rest | On movement, reset 500 ms timer; on expiry with same approved context and no movement, request observation and pointer UIA |
| pre_action | Button down, first scroll tick, non-repeat key down outside text, or first key of text burst after >=500 ms gap |
| next | Arm on each matching button/key release; scroll ends at last tick +300 ms |
| settled | Start per-monitor stability watcher after action end; retain candidate and confirm 500 ms below threshold; give up at 10 s |
| observe | After last input +2 s, then every 2 s while changing; reset schedule on input |
| checkpoint | Confirmed save/checkpoint only; no inference from Ctrl+S |
| final | Retain newest frame before Stop; final-save settled only when a confirmed final-save signal actually exists |

Key/button pairing is per identity/device where available. Repeats do not open extra actions. Multiple overlapping controls keep independent end references. One arriving frame may satisfy several next requests; store separate logical references but one physical image. A held key/button need not wait for unrelated keys/buttons to release.

Scroll bursts are per device/monitor/axis and end after 300 ms idle; later ticks reset only that burst. If exact device attribution is unknown, preserve that ambiguity rather than falsely merge two known devices.

One per-monitor settled watcher coalesces requests awaiting stability. A new action invalidates the current quiet candidate and joins the pending causal references; it does not create another full-resolution stability buffer. Each request retains its own 10 s deadline. Expiry resolves that request; later input cannot keep it alive forever.

### 8.4 Settled details

After the action end, select the first new candidate frame. For each following frame, compare the normalized adjacent-frame change score:
- score >=0.002: replace the candidate, reset quiet start;
- score <0.002: keep the original candidate;
- quiet duration >=500 ms with healthy active capture: resolve settled to the retained candidate.

A healthy desktop with no new WGC frame after an already valid candidate contributes elapsed quiet time; timers can confirm it. If no post-end candidate ever arrives, do not invent one. Pause, missing monitor or capture failure cancels the watcher. An app-hung flag does not establish completion even if its picture settles.

This implements the PDF adjacent-frame threshold, not exact equality to the first candidate. Slow visible change may satisfy settled. Actual completion stays unknown without separate application evidence.

pointer_rest and observe use their timer decision time and current eligible frame, not a fabricated input timestamp. They do not recursively generate before/seen pairs. Actual input-triggered pre_action/pre_motion use the two selectors above.

### 8.5 Readback, hashing, deduplication and priority

1. Capture owner schedules one chosen retained frame into staging.
2. Poll readiness in subsequent iterations with nonblocking Map; a still-drawing result is a retry, not a failure.
3. When ready, copy only width*4 bytes per row into tightly packed CPU pixels; Unmap promptly. CPU row copying is bounded work and its cost must be measured.
4. Release the GPU retention once no other consumer needs it. Pass owned CPU pixels to the PNG worker.
5. Hash versioned dimensions/format/color header and packed rows with full BLAKE3.
6. If a successfully saved identical hash already exists, reuse it. Do not charge another unique-image rate token or encode another PNG.
7. Otherwise apply the new-image cap and persist a PNG through a temporary file; publish its hash path only after success.
8. Resolve all corresponding ordered observation slots and release buffers.

Use one PNG worker initially, which serializes deduplication and avoids duplicate concurrent writes. The successful-image index contains only completed files, never queued intentions. Hash naming is session-local content addressing, not a reason to keep full pixel history in RAM.

Use a rolling one-second window for the target six newly encoded non-exempt images per monitor. Only before images of pre_motion/pre_action are exempt; seen is high priority but not automatically exempt. Pixel-identical reuse is free.

When scheduling or evicting pending jobs, highest priority is pre_motion/pre_action before and seen; then next, settled, checkpoint, start, final; pointer_rest/observe first to drop. Equal priority uses oldest request first. Already saved work is never retroactively removed just to admit a newer job. If the rate cap prevents a higher-priority non-exempt image after earlier work consumed the budget, report a drop; do not pretend the algorithm can predict future triggers.

Every nonsensitive dropped logical observation gets a reason and references. Deduplication suppresses duplicate files, not event history. Cursor position and recording timestamps are not pixels/hash input; actual changes to visible clock text in the captured application remain pixel changes, not metadata.

An Action, Rest or confirmed Save opening during a collector-backed visit carries its exact visit, original time and geometry through the monitor queue. Open only after healthy certified coverage from the shared Receiver/Context path. The four circulating native history slots also bound private frames staged for a collector-backed visit while their proof catches up; pressure and expiry resolve unavailable work. Apply the same temporal selector and calibration in both selection phases. Readback, PNG/deduplication and the still journal check the selected visit and exclusive cutoff again before publishing. Allocate a durable input reference only after input-journal sequence allocation. Allocate checkpoint context and route confirmed fixture Save only while the selected foreground and visit remain current. Compare the original control activation or fixture receipt QPC against the exact visit's exclusive certificate and the existing context and geometry proof. Await proof within the bounded staging window and resolve expiry or closure through the unavailable path. The Recorder control row retains the action, outcome and request ID. Any later checkpoint-context row keeps the original control receipt separately from the publication receipt. The standard routes also respect the source-change latch. A suppressed sensitive trigger leaves no individual observation trace.

## 9. Video slots and segment rotation

### 9.1 Slot selection

For each monitor and active segment epoch:
- Maintain integer slot boundaries from the configured encoding FPS; avoid cumulative floating-point drift.
- Decide a slot when its interval closes. Select the newest eligible captured frame for that slot, or repeat the previous selected frame if none arrived.
- Use local segment PTS derived from slot start and a recorded common-clock offset.
- Preserve selected frame ID/source time on repetition.
- Before the first valid frame, during monitor absence or when queue admission fails, record encoded=false with a reason, no invented segment/frame index.
- Paused intervals have no frame rows. The clock gap is represented by pause/discontinuity metadata.
- Encoding outcome, not mere queue admission, determines encoded=true.

Keep capture source time, processing receipt time and slot PTS separate. Compute lateness using the PDF changed-frame evidence, not from duplicate pixels alone.

During a collector-backed visit, a closed slot can wait within the bounded staging window for its original capture receipt to qualify. Preserve the original slot PTS and record actual conversion availability. The selected frame and full slot span, including repeats, must fit the visit's exclusive proof. Expiry or a cutoff yields an explicit missing disposition. Withdraw an unpublished mux part with an excluded span and account for its affected slots. Standard-source scheduling keeps its existing timing. A late cutoff cannot retract media already published before external detection.

### 9.2 Rotation

One video worker owns encoder and file publication:
1. When the next segment boundary is due, request an IDR on the next submitted boundary frame.
2. Continue consuming ordered encoder output. Route all pre-boundary samples to the current segment.
3. Verify the actual IDR and required parameter sets; requesting a keyframe is not proof of obtaining one.
4. Retain the boundary IDR and its parameter sets. Pause submission and output draining on this worker while it finalizes the old muxer. Capture continues into its bounded video input queue; it never waits for the muxer.
5. Finalize/release/flush/publish the preceding file, then open the next part and write the held IDR first.
6. Resume draining encoder output and submitting queued samples in order. Continue with the same encoder where the format is unchanged; reset segment-local PTS through an explicit offset. Respect the selected synchronous/asynchronous MFT's readiness contract; callbacks only queue bounded notifications. Do not claim output draining runs simultaneously with synchronous Finalize on this worker.

The muxer receives already encoded H.264 samples and must not create a hidden second encoder. Its finalization must not depend on future input to the retained upstream encoder. Start with at most three outstanding owner-supplied video samples per monitor (including queued/in-flight leases); encoder-internal allocations are separate and measured. Pressure marks slots skipped rather than expanding this queue. If an encoder cannot operate under these ownership/readiness constraints, reject it in the startup capability check rather than add an unbounded handoff.

Capture does not wait for file finalization. The video queue is bounded; pressure marks skipped slots rather than growing indefinitely. Encoder delay, video input queue occupancy during rotation and the retained boundary IDR are accounted for. If a requested IDR never appears within the configured bounded rotation allowance, diagnose video failure and preserve the unfinished part; do not start an undecodable segment.

Use the PDF fixed VBR path initially; optional quality-mode switching is unnecessary complexity for M1. Startup capability checks select HW or SW before recording. No runtime repair/recreate/fallback on a failed encoder.

### 9.3 Display transitions

DPI/desktop-position changes update geometry metadata without recreating the pool or segment when pixel format/size is unchanged. Size/rotation/HDR changes close the old epoch, resolve/drop old pending selectors explicitly, then create new format resources and segment. Old frames/jobs retain old geometry identity until released.

Monitor disappearance ends that monitor's capture epoch; other monitors continue. Active nonpaused slots for the missing selected monitor are recorded unavailable. Return creates a new capture item and epoch under the same stable monitor identity. First-frame and observation logic must not use a stale HMONITOR.

A true GPU device failure is diagnosed under the scope addendum, with no automatic device rebuild. Do not confuse expected topology reconfiguration with fault recovery.

## 10. Journals, disk, errors and Stop

### 10.1 Journals and pressure

Journal writes are append-only with one owner. Normal buffered flush every second; OS flush every ten seconds and on the PDF boundaries. Check and propagate each write/flush result. Never report a successful boundary flush when it failed.

Input/context/app queue targets remain 65,536/8,192/8,192 entries. Metadata and pixel queues have separate byte limits. Counter storage outside a full queue allows later gap publication. The inability to write the health journal must not recursively generate an unbounded sequence of health errors.

Check disk availability before Start and periodically; use the PDF minimum/reserve where expected duration is available. Without M2 task duration, use the PDF minimum and record duration-based reserve as unavailable rather than inventing hours. Low disk opens a pause reason. Resume requires explicit user action and more than the PDF resume threshold.

Reports use a bounded diagnostic snapshot: component, operation, error/code, times, configuration and queue counters. Never include buffered password text, arbitrary captured pixels or clipboard contents merely for diagnostics. Write the report into the selected output directory; if writing fails, display report-unavailable and the original error without redirecting it elsewhere. Use a temporary directory only before an output directory has been selected. No retry loop and no automatic transmission.

### 10.2 Stop sequence

1. Set state Stopping and a single Stop boundary/deadline (normal drain budget up to 10 s).
2. Stop admitting new application input and future timers. Retain final pre-Stop frames immediately.
3. Close published held inputs; close redacted visits; resolve pending classification conservatively.
4. Ask producers to finish their admitted prefix and stop. Cancel unresolved future next/settled requests with explicit reasons; do not wait ten seconds merely for a new future image.
5. Drain accepted image/video jobs within the deadline. Collect final overflow counters and drop outcomes.
6. Finalize/release/flush/publish required video segments.
7. Resolve ordered journal prefixes, append final outcomes and flush all journals.
8. Reconcile the closed inventory and flush the staged manifest. Immediately before atomic Complete replacement, arbitrate authorization against the Stop deadline under the lifecycle lock, with the controller failure and admitted prefix checked. Release the lock before the Windows call. An incomplete disposition follows the existing failure and cancellation path.

A final-save settled observation is drained only if its quiet interval was already confirmed before the Stop boundary. Its image may finish encoding during drain. An unconfirmed watcher is cancelled at Stop with an explicit reason; no post-Stop capture window is introduced, and Stop itself does not initiate an application save.

If the deadline expires before authorization, the controller reports timeout and the metadata owner never submits the Complete replacement. Preserve the incomplete manifest and diagnose the failure. Owners of blocking IO keep their resources until they return.

If Complete publication is authorized before expiry, keep the attempt active and show final publication pending when the waiting budget expires. Use the Windows outcome for the final status. The UI remains responsive and the recording lease stays with the existing owner through its exit. Pending publication uses the same single replacement call, with no retry, compensating manifest or automatic repair.

Shutdown and suspend retain the shorter PDF budgets for their existing operations. Apply a shortened Stop deadline to admission when the session-end timestamp is visible, even before the controller processes its wake. An already authorized Complete replacement retains its Windows outcome. The native session-end handler continues to use its bounded wait and can return while the recording owner still holds the attempt. Preserve unfinished parts when required work cannot finish. Ordinary rotation/sharing-violation publication retries must be bounded by the relevant finish deadline, never an infinite retry.

Stop before admission cancels pending consent, preparation and startup. Once admitted, Stop closes the current source and cancels any outstanding handoff. Stop from pause keeps the earlier admitted cutoff. When pausing a collector-backed visit, service the grant close handshake within its 750 ms closing deadline. The subsequent input/context catch-up to the retired cutoff has a separate 250 ms bound and ends sooner if the prefix arrives or Stop interrupts Pause. On Stop, the collector cleanup limit is 750 ms shortened by the session Stop deadline. Service the control and data lanes within that bound. A terminal host acknowledgement confirms cleanup after the active grant has reported closed. Continue independently receiving lane progress until the effective cutoff is received, both lanes exit, or the deadline arrives. Lane exit or the deadline can leave a shorter certified prefix. Settle only the exact retired visit below the earliest safe exclusive boundary and account for an uncertified remainder. If the required collector fails, retain the first cause, bound source shutdown and finalize the provable failed prefix through the existing owners. The operator sees a retained package path/count only after failed-manifest publication is confirmed. Reader-side input staging pressure tightens the retired visit cutoff at the original first-uncovered QPC. At Pause or Stop, publish the retained loss through the context boundary path using a monotonic journal receipt while keeping the original QPC, exact visit and per-producer native gap ranges in the payload. Stop while paused retains the earlier admission cutoff and uses the Stop receipt for newly reported loss. Optional source evidence describes actual launch, attachment, readiness/registration, selected coverage, blind and excluded tails, and observed stop. The standard helper retains its own evidence fields.

## 11. Concrete scenarios and expected results

| Scenario | Required result |
| --- | --- |
| Single click, unchanged pixels | Before/seen selected by time; next may reference a later capture with identical pixels; one file reused, multiple logical observations allowed |
| Cursor movement only | No cursor baked into captured pixels; no duplicate image files solely from cursor position metadata |
| Click changes a panel | Keep eligible pre-action frame and first post-release capture; settled references the beginning of the confirmed quiet period |
| Fast down/up before another frame | Preserve pre-action reservation; next waits for first eligible new frame, no later frame used as before |
| Drag with held Shift | Preserve held motion and modifiers; no still per motion sample; independent key/button release references |
| Rapid overlapping keys | Independent key lifetimes; share physical images when selectors coincide; bounded waiting and explicit drops |
| UIA completes after focus changed | Result cannot classify new-focus keys; old unresolved sensitive keys remain conservative |
| Password visit | One aggregate, no per-key input/observation/drop timing leaks |
| IME composition | Composing keys not emitted as invented committed text; use finalized commit evidence or unknown |
| Equal before/seen bytes, different frame IDs | Distinct provenance can refer to one image hash |
| No new WGC frame on static desktop | Not a failure; video repeats with original provenance; no invented next capture |
| More than 256 pending jobs / pixel limit | Apply priority/drop rules; capture and input remain responsive |
| Software encoder selected at startup | Video 15 FPS, capture/still timing unchanged |
| Pause while key/button held | Synthetic closure before pause; no ordinary input inside pause; resume is a new video epoch |
| Monitor changes during readback | Old job retains old size/version; new requests cannot select old-geometry history |
| Stop with pending PNG | Finish admitted work within budget or explicitly drop; manifest does not reference an unsaved successful image |
| Helper/encoder fails | Error and report, truthful incomplete/degraded data status; no automatic recovery |
| Output disk write fails | Original failure remains visible even if diagnostic output also fails |

## 12. Verification

Static verification must trace each producer to its consumer, each retained resource to its release and each request to a terminal result. Review the scenarios above, especially queue saturation, asynchronous completion ordering and pause/Stop interactions.

Compilation and runtime tests require the separately established execution authorization. When run, distinguish source review, compilation, functional results and measured hardware results. Use supported Windows versions and pen hardware for their applicable scenarios. Timing and full T20 acceptance require the corresponding measurement setup.

Verify timing, GPU-memory budgets and Windows scenarios against the [M1 acceptance criteria](SCREEN_FIX_M1_SPEC_ADDENDUM.md#6-revised-m1-acceptance-mapping).

