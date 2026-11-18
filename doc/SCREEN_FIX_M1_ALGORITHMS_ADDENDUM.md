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
- WGC source time is mapped using CLK-3, not the video PTS. WGC `SystemRelativeTime` is the DWM present (vblank) time of the frame, which can lead the frame's dequeue by up to one refresh interval (§13.14).
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

1. Classify the actual approved foreground at countdown completion or during recording. Require an exact process incarnation, foreground window/thread, interactive desktop, session, access and approval proof. Suppress stale or uncertain observations. An ordinary approval then belongs to that process instance: another top-level window of the same process continues it under the same generation (section 13.14), while an elevated approval stays bound to its visit's window. An elevated Recorder uses the standard source when the non-SYSTEM target satisfies the shared interactive user and session checks, has integrity at or below the Recorder’s actual integrity, and passes the same-sign-in or reciprocal linked-token check. This direct-access decision also feeds the standard context and point checks. An inaccessible elevated hit away from the foreground cannot launch the collector.
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

IMG-3's pending-still targets are 256 MB per monitor (decimal bytes, 256,000,000) and 256 jobs; M1 uses tighter bounds: one PNG worker with a 12-job queue and at most 6 new images per second (spec addendum A7.10). Count pinned pending images as well as CPU pixel buffers conservatively; do not use the CPU queue limit to justify unlimited GPU pinning. Temporary readback duplication counts until one representation is released.

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

Correlation quality, closing certification, hook-only attribution, motion and wheel association and boundary key state are specified in section 13.

### 6.2 Sampling and foreground evidence

Keep all key/button transitions except PDF control/password exclusions. Keep all motion samples while a button is held or a configured stroke is active (configured stroke tools come from app packs in M2; M1 applies the held-button rule, spec addendum A7.10). Otherwise emit the first sample immediately, retain the latest sample in each 1/60 s sampling window, and flush the last pending sample on idle, transition or boundary. Report thinning/coalescing counts.

Foreground approval must be established for the event's receipt context, not from a later UIA result. Capture a lightweight foreground identity with normal input receipt processing; hook callbacks remain copy-only. If a hook-only event's receipt-time foreground cannot be established from available ordered context evidence, conservatively suppress it as approval-unknown rather than attribute it to a later foreground. A pointer or other non-key record needs only the approved process instance in the foreground at its receipt. A key also needs a known, classified focus, and its privacy follows the actual focused window (section 13.4). Do not log unapproved window details.

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

Section 13.8 records how undetermined editability is classified (D-4).

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
- Decide a slot under the source-time watermark of section 13.9, which amends the earlier rule "when its interval closes". Select the newest eligible converted frame for that slot, or repeat the previous selected frame if none arrived. One conversion per monitor is in flight; a frame arriving while it is busy marks its slot with `conversion_pressure`. With software encoding only the first approved frame of each slot is converted, at once, and later frames of the slot are counted as `video_software_paced` (SPEC addendum A7.2, A7.3).
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

## 13. Remediation algorithms (2026-11-17)

This section records the algorithms of the M1 remediation plan (`SCREEN_FIX_M1_REMEDIATION_PLAN.md`) and the user decisions D-2 to D-10. Where it differs from an earlier section, this section applies. Journal field names change with the schema stage (D-1 A) and are documented separately with it. The behaviour below was implemented and reviewed statically. It has not been compiled or executed.

### 13.1 Terminal outcomes and the loss ledger

Every item an owner accepts reaches exactly one terminal outcome:
- Persisted: the row or file was written.
- Suppressed: withheld by approval, privacy, control or pause policy. It is counted per class only.
- Dropped: lost to capacity, malformation, deadline or source loss. It becomes a ledger entry and a typed `gap` row in the same stream.
- Unavailable: a required observation could not be made. It is an explicit row where the stream defines one (still, element, focus), otherwise a ledger entry.

Correlation quality is never a terminal outcome. It is metadata on a persisted row.

Ledger:
- Each stream ingress owns the ledger. Its key is (reason, producer); reasons and producers are closed typed enums. At most one entry per key is pending, outside the data queue.
- An entry holds a count (known, or unknown; unknown stays unknown when merged), the first and last receipt of the lost items, and optional native sequence and QPC ranges. Merging sums counts and widens ranges.
- A loss of a password, possibly sensitive or approval-unknown item is Sensitive, and so is a key record lost before its visit is classified (pointer and HID records keep their ranges). It is counted only, carries no time or sequence range and never becomes a gap row.

Emission:
- Pending entries are written as gap rows before the next row of the same stream, in ascending first receipt.
- A gap row's `recv_t_us` is its admission receipt: the maximum of the triggering receipt and the stream's last receipt. Its range fields keep the original loss times.
- At a pause boundary pending entries precede the pause row. Losses noticed while paused follow the resume row with their original ranges. At close they precede the final health rows.
- Capacity: before a data row, pending gap rows use the normal data capacity. If it is full they stay pending in the ledger (the data row itself becomes a `queue_overflow` loss), and an idle producer wake offers them again. Gap rows are emitted in reserved capacity before boundary rows and before a health row, by a blocking priority send when a waited flush finds the queue full, and at close in the ingress tail. Health rows always use the reserve. The reserve (24 entries above the data limit) is shared by gap batches, boundary rows and health rows. When it is full, a gap batch stays pending in the ledger, and a boundary or health row fails with a distinct `reserve` journal failure (F). Failure to append one is a journal failure (F) and produces no further health row.
- The frames stream never records a loss. Video losses are slot rows with a reason (13.14).

Health rows:
- They are written when the condition occurs, never with a null `recv_t_us`. A condition before the session clock exists goes to diagnostics and the manifest only.
- While paused they are counted and reported with the resume row, or at Stop in a `paused_summary` row.
- Capture and video health reported after the input/context loop has ended (for example a still write failure during the Stop drain) is written with the final context rows. If the context journal does not exist, it is counted in the manifest `omitted_health`. If the input/context loop itself failed, so that its hand-off index is unknown, such late records are neither written nor counted; that attempt has already failed (F).
- More than 256 capture health records are counted in `omitted_health` instead of written.

The manifest `health` block holds pauses, gap rows, blind intervals, dropped stills, `recovered_prefix: false`, per-stream dropped totals by reason and suppressed totals by class, the activity losses (13.2) and `omitted_health`. The reader reconciles it with the stream summaries.

### 13.2 Fault classes

The controller owns the classification. Owners report typed `FaultReport` values; none decides the attempt outcome.
- F, attempt failure: the Stop path runs and the first cause is retained.
- L, activity loss: the activity stops for the rest of the attempt, a health row and blind or unavailable intervals are recorded, and the UI shows "Recording degraded". L-required covers input and the video of one monitor. L-auxiliary covers stills/PNG, UIA, clipboard, cursor assets and identity resolution.
- G, recorded loss: a ledger entry, an unavailable row or a bounded blind interval.

| # | Condition | Class |
| --- | --- | --- |
| 1 | Journal append, flush or close failure | F |
| 2 | Manifest publication failure; a required segment unfinalized or unverified at the Stop deadline (including a segment interrupted during IDR rotation, once the Stop deadline has expired) | F |
| 3 | Clock or controller failure | F |
| 4 | Capture-thread failure that is not a topology change (for example GPU device loss) | F |
| 5 | Admitted collector source failure (host, transport, authentication, protocol, lease, witness regression, readiness deadline, required access on the current grant, failed standard-helper check on return) | F |
| 6 | Collector provider call still running at request deadline + 750 ms | F |
| 7 | Collector input-lane write outstanding beyond the 500 ms coverage stall, or control-lane write beyond the lease | F |
| 8 | Collector request deadline expiry where the call returns before deadline + 750 ms | G |
| 9 | Encoder failure or rejected segment verification on a monitor; a segment close or rotation part failing before the Stop deadline expired; encoder release failure at a format change or at Stop | L-required (that monitor's video) |
| 10 | Helper exit or transport break | L-required (input) |
| 11 | Input producer progress stall | G (blind interval) from 250 ms; L-required (input) from 5 s |
| 12 | Required Raw Input registration loss or required hook loss (D-2 B) | L-required (input), blind until Stop |
| 13 | Opening or resume resync not possible; non-ordinary foreground | G (bounded blind interval) |
| 14 | UIA client open failure, or one UIA call blocked longer than 1 s | L-auxiliary (UIA) |
| 15 | Identity resolver blocked in one native call longer than 2 s | L-auxiliary (identity resolution) |
| 16 | PNG encoder exit | L-auxiliary (stills) |
| 17 | Clipboard worker exit | L-auxiliary (clipboard) |
| 18 | Queue overflow, malformed packet, unknown device, optional device failure, display enumeration race, identity failure for one spelling, hotkey stall, late suspend finalization, per-request UIA, IME or element timeout | G |
| 19 | A segment's verification result not received within the verification allowance | L-required (that monitor's video) |
| 20 | Frames staging overflow after one slot interval of waiting | F |
| 21 | A video worker's frames mark does not advance for T while Recording | F |

Rules:
- A condition escalates only at the boundary listed in the table.
- The result message of a failed attempt names the first F. The primary cause in diagnostics is the first F or L; later reports are attached as secondary causes. F reports beyond 32 are counted, and identical F causes are reported once. A Stop-deadline timeout never replaces a recorded cause.
- The disposition takes the first matching line: any F gives `failed`; L-required video or the selected-monitor rule (13.11) gives `video_unavailable`; L-required input or an input-certification shortfall at Stop gives `input_unavailable`; otherwise `recorded`. L-auxiliary and G never change the disposition; they appear only in `health`.
- The component is the reporting owner: COM/MF initialization and monitor preflight report video; topology, display enumeration and OS capability report capture; approved-application preflight reports context; hotkeys, disk, environment and clock report the controller; the indicator and window tracking report the interface; the input worker reports the receiver; helper errors report the input helper.

Implementation notes:
- Row 16: the PNG worker also owns the still journal. An encoder panic is caught; it is reported once as an L-auxiliary loss with a health row, and this and every later job resolves as `still_dropped` with reason `png_worker_unavailable`. The still journal and the recording continue. A still journal failure, or an exit of the worker outside the encoder, stays F (row 1) with no extra L report.
- Cursor assets: a panic of the cursor asset owner is caught. The assets it already published keep their inventory entries; later assets are unavailable, and the context owner reports one L-auxiliary loss on its next health poll. A panic after the context loop's last poll keeps the inventory but is not reported.
- Row 7: the collector writes status records on its owner thread with the 750 ms cleanup bound as their write deadline, the same bound within which the collector supervisor requires that thread's progress outside cleanup; a write still incomplete then fails the lane. A stalled Recorder command or query write is a transport failure, not a readiness-deadline expiry.
- Row 17: a clipboard worker exit is reported once as an L-auxiliary loss with a context health row. No replacement worker is started, so no later clipboard events are produced.
- Rows 20 and 21 belong to the per-monitor frames lanes (13.14).

### 13.3 Closing certification of the input prefix

- An item with receipt t below the cutoff is decidable when every producer's progress is at or beyond min(t + window, cutoff), or the producer has delivered its final progress. At a closing boundary every item below the cutoff is decided with the candidates present; missing partners are treated as at expiry.
- `Discontinuous` marks only items whose window crosses a recorded gap, barrier or device removal. It is metadata, not a reason to suppress.
- Waits for final progress: the existing 250 ms at Pause; at Stop, the Stop deadline minus a finalization reserve (`STOP_FINALIZATION_RESERVE` = 2 s). Under the 10 s Stop deadline about 8 s remain for certification; under the 3.75 s session-end budget about 1.75 s.
- Items after the last progress of a producer that did not deliver final progress are Dropped with reason `producer_stall` over [last progress, cutoff). At Pause this range becomes a blind interval before the pause row (G). At Stop it is recorded as `uncertified_closing_tail`, an input-certification shortfall (`input_unavailable`).

Matching outcomes (key, button and wheel clusters): a cluster is a connected component of compatible raw and hook records within ±20 ms, in one admission epoch and not separated by a gap, barrier or device removal. One raw and one hook, mutually unique, give one paired row (`matched`, or `missing_scan`). Hooks alone are hook-only (13.4). Raw alone gives one row per raw record (`unmatched`). Otherwise the component gives exactly one row per raw record with `correlation: "ambiguous"`, hook-only fields null with reason `ambiguous`, and cluster sizes; excess hooks are suppressed as `cluster_excess_hook`. Ambiguity does not spread beyond its component. Every record ends in exactly one row or one count.

### 13.4 Hook-only foreground attribution

There is no separate Receiver-owned timeline. The Context owner's visit history plays that role (`Context::hook_receipt`). It holds the current approved visit and up to 32 ended visits, each with its start and the receipt through which a context sweep verified it unchanged. `Context::prune_visits` drops ended visits whose end lies more than the 16 ms uncertainty before the published input prefix, except the visit of the active key focus.

A hook-only record with receipt r is approved when:
1. one approved visit started at or before r − 16 ms and was verified unchanged through r, so no change lies in [r − 16 ms, r];
2. that visit has ordinary access (an elevated visit covers only receipts inside its certified visit);
3. a context sweep has processed notices through r. Until then the receipt is pending.

If condition 3 is not yet met, the record waits within the existing classification wait. Otherwise a failed condition suppresses it as `approval_unknown` (Sensitive).

Visits and focus generations stay window-level. For a pointer record, a focus change inside [r − 16 ms, r] is attributed to the later visit when both visits are ordinary visits of the same approved process instance and the later one replaced the earlier in the same sweep, with no other foreground in between (rule 6); this includes a switch to another top-level window of that process. Any other switch inside the interval, or a seam with an elevated visit, leaves a hook-only record `approval_unknown`. A record with Raw Input evidence (raw only or raw plus hook) is checked against the approved process instance at its raw receipt instead (`approved_receipt` in `input/gate.rs`). For a pointer or other non-key record that is enough, including a record whose focus is historical or unknown. A key with a hook part additionally needs the current visit's focus; a historical or unknown focus makes it `approval_unknown`.

Keyboard records (keys and VK_PACKET) also require:
4. one visit and one focus generation covering [r − 16 ms, r];
5. that generation's classification resolved within the 500 ms classification deadline. Non-text keeps keys, editable follows text rules, and password, possibly sensitive, unresolved or expired goes to the redacted aggregate.
6. If condition 4 fails because the focus changed inside the interval while the foreground stayed in the same approved process instance (two ordinary visits, the later replacing the earlier in the same sweep, with no other foreground in between), the record is counted into an adjacent open redacted aggregate, or else as a Sensitive `focus_uncertain` suppression. Any other change inside the interval makes the receipt unknown (`approval_unknown`).

Origin follows INP-7 precedence: promotion signature, then hook injection flags (`injected`), then device evidence. An injected hook may pair only with a NULL-handle raw packet of the same kind and identity. Injector hints are serialized by hint name. `late` is null for injected hooks.

### 13.5 Motion and wheel association (D-7 A)

- Every approved hook motion is a sample; ALG section 6.2 sampling still applies.
- Raw relative packets received in (previous hook sample − W, this sample + W], not yet attributed, are attached as `delta` and `coalesced` when exactly one raw device produced them; at most one absolute packet is attached. Packets of several relative devices, or several absolute packets, make the sample's raw evidence unavailable with reason `ambiguous` (the row's correlation is `ambiguous`). Without any raw packet the reason is `not_observed`, or `blind` when the window crosses a recorded gap. A raw packet received within +W after a hook sample is attributed to that sample, so in dense motion a delta can lag by one sample.
- Unattributed raw deltas go to the per-second raw-motion evidence in `input_health` and are not counted as cursor moves. No delta is attached to two samples.
- While the pointer hook is missing or lost, raw packets drive sampling.
- Wheel: in a maximal run of same-axis records without a gap or barrier, pair the k-th raw with the k-th hook when the run's raw records come from one device and the counts are equal. Otherwise each hook record is published with its raw value unavailable for reason `ambiguous` (correlation `ambiguous`), and the run's raw records are absorbed by its first row. A run of one producer only stays unpaired (`not_observed`). A run capped at 32 records is cut only where its raw and hook counts are equal.

### 13.6 Ordinary-key state across boundaries (I4)

- Each resync reads the state of all virtual keys into private state only (`KnownUp`, `KnownDown` or `Unknown`). Ordinary keys get no synthetic edges.
- The first down of a `KnownDown` or `Unknown` key after a boundary, and its later autorepeats, are published with `repeat: null` and `held_across_boundary: true`. It is not a press, does not open a `pre_action` trigger and does not enter the published held state.
- A key up without a published down is published with `press_unavailable: true`.
- Text inference ignores `repeat: null` downs.

### 13.7 Liveness and blind intervals (I5)

- An input producer stall opens a blind interval after 250 ms of awake time and becomes L-required input at 5 s. Only records of the stalled producers are dropped; hook-health transitions inside the stall interval are not counted.
- A malformed packet is skipped and counted (G), not fatal to the helper.
- `input_health` rows report per-second coalesced, thinned and suppressed counts. The per-second suppressed counts exclude the privacy-adjacent classes (approval unknown, not approved, control uncertain, cluster excess) and the AltGr companion key, which is counted only in the stream summary.
- A helper failure noticed after the pause prefix is published writes no runtime input health row; the final activity-loss row remains.
- Suspend-excluded elapsed time comes from the power marks of the session; a mark without a QPC gives an unknown interval, never zero, and an unknown interval never produces a fault.

### 13.8 Focus classification (D-4; clarifies CTX-9 against section 7.1)

- D-4a: a successful read with IsKeyboardFocusable true and neither Value nor Text pattern gives `editable: false` (non-text, keys kept) for every control type.
- D-4b: a present pattern whose read-only attribute is unavailable gives `editable: null` and a redacted visit. This includes the case where Value pattern presence itself is unavailable and `ValueIsReadOnly` is unreadable.
- D-4c: a mixed Text read-only attribute with IsPassword known false gives `editable: null`; keys are published with `typing: "unknown"` and no text is inferred.
- D-4b and D-4c have no journal sub-reason. Both give `editable: null`; the distinction is the text target class: possibly sensitive (redacted) for D-4b, typing unknown for D-4c.
- D-4d: `HasKeyboardFocus` false marks the read stale; it is reissued once within the classification wait and redacted if the wait expires. `HasKeyboardFocus` unsupported is resolved when the element came from the focused-element query and a native focus snapshot taken from `hwndFocus` confirms the same window.
- Unchanged: read failure or timeout, IsPassword unknown and password give a redacted visit.

Read corrections (C1):
- Value read-only with Text writable gives `editable: true` (CTX-9 "either … is false"). Value read-only with Text undetermined gives `editable: null`, redacted. A present Value pattern with its attribute unavailable and Text read-only or absent gives `editable: null`, redacted.
- `UIA_E_TIMEOUT` and an exhausted call budget are reported as `timeout`, not `unavailable`. Each provider call gets the remaining budget, with a 10 ms minimum.
- When no `hwndFocus` exists, the `hwndActive` fallback serves keyboard layout and focus identity only. Its password evidence is unknown, the UIA read is treated as stale, and the target is possibly sensitive. Clipboard focus checks exclude it.
- Invariant for every consumer of `FocusAtReceipt.password_style`: `None` never means "not a password". Only `Some(false)` is password-safe.
- An IME state without a focus snapshot is unknown.

### 13.9 Slot decision watermark (D-10 A; amends section 9.1, with VID-4)

- Slot k [start_k, end_k) is decided when the capture owner has delivered a frame with source time ≥ end_k, or when now ≥ end_k + G.
- G is one capture pacing interval plus the conversion allowance, capped at two slot intervals. It is recorded per segment as `slot_decision_grace_us`.
- A frame with source time in the slot is eligible whenever it becomes available before the decision; receipt before end_k is no longer required. `late` keeps its VID-4 meaning. Frames arriving after their slot was decided are counted as `arrived_after_decision`.
- Same-size geometry changes (DPI or position) keep the repeat source. A frame that arrives before the epoch timeline is published is held for that timeline or counted as `timeline_pending`.
- Video capacities derive from the effective FPS: segment pictures and staged slots are computed from max(requested FPS, software FPS), and staged slots cover two segments.

### 13.10 Segment verification and encoder selection (V3, V4)

- The video worker performs rotation (IDR, finalize, flush, next part) and hands the closed part to a per-monitor verifier thread through a queue of two. Encoding continues. A full verifier queue makes the worker wait and is counted.
- The verifier decodes the segment and checks timing and dimensions. Only after success, and an authorization check immediately before the rename, is the `.part` renamed to `.mp4`. A verifier that cannot initialize COM returns its cause as the first part's verdict and exits; every part stays `.part`. An already published `.mp4` is never deleted on a later elevated revocation; the remaining race window is benign because the segment contains only frames admitted while the visit was approved.
- A rejected verification is L-required for that monitor (row 9). A result not received within the verification allowance resolves the segment's rows as `segment_unverified`, keeps the `.part` and is L-required (row 19). Only an unfinished required segment at the Stop deadline is F.
- Display read failures give unavailable fields instead of failing. An output whose color mode or rectangle cannot be read affects only that monitor; another selected monitor still resolves. HDR without an SDR white level uses the system BGRA8 conversion with a health row. Encoder selection failure at reconfigure is L-required for that monitor (capability selection, not recovery). Rejected hardware candidates are recorded with their causes (`encoder_candidate_rejected`), also when the selection fails because no encoder qualifies. The encoder self-check pictures are converted on the capture thread.
- Pause: while rows of the closed epoch await verification, the paused worker applies arrived verdicts and publishes each verified segment (`.part` → `.mp4`) and its rows at once, instead of at Resume or Stop. The overdue check (row 19) is not applied during the pause, so a pause does not change which parts are abandoned. A video-loss report from a rejected verification during the pause can therefore precede the resume rows.
- Suspend: a monitor that has not closed its open segment within 2 s of `PBT_APMSUSPEND` writes `suspend_finalize_late` and finishes after wake (D-5 A). The encoder watchdog, the elevated prefix wait and the conversion deadline restart on a suspend.

### 13.11 Session and completion rules

- D-6 A: complete requires usable video for each selected monitor in each interval in which it was present and admitted. An admitted interval of a present monitor counts as without video only if it has no encoded slot and contains a slot that starts later than the interval start plus G; shorter intervals are counted in `short_intervals_without_video` and do not affect Complete. Approval-suppressed slots lie outside admitted intervals.
- Hotkeys: the hotkey pump renews its progress mark at least every 50 ms, also without messages. A hotkey progress gap (over 250 ms) is measured as suspend-excluded time and records a control loss; it no longer disables hotkeys. There is no separate registration-loss health record: a `RegisterHotKey` registration belongs to its thread and is lost only when that thread stops, which is already detected and shown.
- The countdown is held during lock, secure desktop and sleep. An admitted failure is reported as a retained recording with its package path when the failed manifest is confirmed.

### 13.12 Stills (P1–P3)

- The PNG inbox takes a short blocking lock; `capacity` is reported only when the queue is full and the new job does not outrank the lowest one.
- Drop reasons are set at their cause: a save without an eligible interval gives `admission_closed` or `save_interval_changed`; a pointer rest gives `admission_changed`, `geometry_changed` or `frame_not_approved`; an approved private request with no selection gives `no_approved_interval` or `adjustment_failed`. Fallback reasons remain only for a handoff disconnect.
- A PNG write failure gives one context health row (`still_write_failure`) per episode of the same failing operation; each still row keeps its operation and native code.
- At the drain deadline a finished PNG job keeps its result; an unfinished job is cancelled before its write and before its rename. An image published after its slot gave up is kept, unreferenced. The manifest PNG counter `unreferenced_images` is exact: every inventoried image that no journaled still row names in `file` or `seen_file`, plus revoked images whose removal failed.
- A `pre_motion` envelope, with its still-row reservations, opens only for the first motion after 300 ms without motion, decided at receipt in the Receiver. Later motion reserves nothing.
- Retention: pending private selections take part in reclaim by IMG-7 priority, newest first among equal priority. Pending private selections hold at most two distinct frames, so one extra hold stays available to public requests.
- `start` returns `history_lost` when an eligible frame after the boundary was already evicted, instead of a later frame.
- Scroll ticks without device attribution form one unknown-device burst per monitor and axis; known devices are never merged.
- One readback and one PNG job serve every slot that selected the same held frame. The job uses the highest attached priority, the rate exemption if any attached slot is exempt and the least restrictive coverage; each row is still checked against its own coverage. The work is released with its last attached slot. The staging texture is kept per frame extent and recreated only on an extent change or epoch cancellation.

### 13.13 Output inventory (P4)

- The PNG owner and the cursor asset owner report each file they publish (attempt-relative path named by its digest, byte length, width, height). The manifest lists them as `images` and `cursors`.
- The reader resolves a still or cursor image only if it is listed, and checks the listed size and dimensions against the file.
- A complete package requires both lists. Complete packages recorded before this change, without the lists, fail validation; this follows the v6-only reader decision (D-1 A).

### 13.14 Frames stream order and receipts (remediation plan §6.5)

Receipt rule (amended, accepted by the user 2026-11-17). Rows are per monitor, in slot order across all epochs of the attempt. For row k, d_k is the slot decision receipt (kept as `slot_decided_t_us`) and e_k is the slot end, limited by the epoch cutoff when the epoch closes inside the slot.
- A row that newly selects frame f has `recv_t_us` = the capture receipt of f. If that receipt is below the previous row's `recv_t_us`, f is not selected: it is counted in `frames_rejected` and the slot repeats. This replaces the former attempt failure on a frames order violation. (`frames_rejected` also counts frames that were not available before the decision or not accepted.)
- A repeat or a row without a frame has the ordering receipt `recv_t_us` = max(previous row's `recv_t_us`, e_k); the first row of the attempt uses e_k. It advances on a static screen and during absence. A repeat keeps the repeated frame's identity and source time.
- Frame times (amended 2026-11-18 after manual tests). The WGC `SystemRelativeTime` is converted to session time with a single rounding from the exact QPC-scale difference (`ClockOrigin::wgc_time` in `protocol/src/time.rs`), as receipts are. It is the DWM present (vblank) time and in practice leads the dequeue by about one refresh interval (14–15 ms at 60 Hz), so it cannot be bounded by the dequeue QPC.
  - `source_t_us` is the present time. It is monotonic per monitor and is what the user saw, so it places the frame in its video slot and bounds a preceding still from above (the content was composed no later).
  - The frame's capture receipt (`capture_recv_t_us`, and `recv_t_us` of a row that newly selects it) is max(dequeue, present). Source ≤ receipt therefore holds by construction.
  - A present time more than 42 ms (`MAX_PRESENT_LEAD_US`) after the dequeue is not a present time; the frame is rejected and counted as `rejected_time_or_epoch`.
- Frame approval (amended 2026-11-18). An ordinary approval belongs to the approved process instance. A foreground change to another top-level window of that process continues it: the generation and `since` are kept, so the video is not cut and still openings keep their interval. The continuation is `interrupted`, and a new generation starts, when the context poll sees a lost notice, a desktop switch, or a foreground notice for a window of another process, an unreadable window or a Ghost window, so a brief visit to another application between two polls still ends the approval. A non-approved foreground, a new admission epoch or a stale observation (`FramePolicy::publish`) also ends it. An elevated approval stays bound to its visit and window. The content of a frame was composed no later than min(dequeue, present), its content time. The ordinary frame approval must contain that time. The foreground observation behind the approval is refreshed every 2–5 ms on the input pump and usually lags the content time by less than one refresh, so a frame whose approval is current but not yet observed through its content time waits for the next observations, up to 20 ms after its content time (`MAX_APPROVAL_CATCH_UP_US`), instead of being dropped. A frame whose approval ends or changes meanwhile is rejected and counted as `rejected_approval`. Elevated frames keep the staging and certificate path.
- A transient approval read (`Contended`, or an approval observed after the reader's own clock sample, `NewerThanReceipt`) is retried for every source, not only for elevated visits: frames within the same 20 ms bound, still openings and saves within `ELEVATED_FRAME_WAIT_US` (250 ms) of their receipt. Before, an ordinary still opening that hit this race was dropped as `no_approved_interval`.
- So every monitor's receipts are non-decreasing by construction. In normal operation no frame is rejected: WGC delivers a monitor's frames in source order and receipts are stamped in processing order, and every later slot's frame has source time, and so receipt, at or after e_k.

Merge (no loss, no blocking between monitors):
- Each video worker decides every non-paused slot in real time, including while its monitor is absent and after its own video L fault. Rows go to a bounded per-monitor lane of ceil(T × effective FPS) rows; a row that does not fit stays in the worker's staging. After each enqueue the worker publishes an atomic mark (epoch, receipt, next slot); all its rows below the mark are already in the lane.
- The merge thread releases the smallest lane head by (`recv_t_us`, monitor, slot) while every other monitor's constraint admits it: the monitor's mark for the current epoch, the opening floor R until that monitor publishes a mark of the new epoch, and no constraint after its closing mark until the next opening. A row the writer cannot take stays at the lane head.
- Opening: at Start and every resume the controller sends Opening(e, R) to the merge thread and waits up to 1 s for its acknowledgement before any video worker sees epoch e. The merge thread does no file IO, so a missing acknowledgement is a journal failure (F).
- Segment flush: a segment whose outcome is final asks for a flush of the frames journal through its last row, which waits until the merge has released that row. A segment with no rows asks for the prefix written so far, at once. Requests still pending when the merge finishes are flushed then.
- Row 20 (F): staging that still exceeds its bound after one slot interval of waiting for lane space.
- Row 21 (F): the merge thread's watchdog reports a worker whose mark does not advance for T = segment duration + IDR allowance + verification allowance + one slot, measured as suspend-excluded elapsed time; an unknown interval restarts the measurement. It applies only while the controller is Recording. It skips a monitor whose full lane still holds rows, because those rows wait for another monitor's mark. It also watches an older epoch's mark that was never closed: at a pause a worker publishes its closing mark only when no staged row remains, so while rows of that epoch still await verification its last mark keeps constraining. On a report the lane is closed at its last mark, later rows of that worker are discarded and the undelivered slot range is recorded in diagnostics and the manifest.
- Each row also keeps its publication time as `published_t_us`; `recv_t_us` is never taken from publication time.
