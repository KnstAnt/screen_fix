# Findings, area A: video pipeline (`core/src/platform/windows/`)

Files covered: video.rs (1913 lines), monitor.rs (1017), encode.rs, encoder.rs, h264.rs, mp4.rs, slots.rs, readback.rs, gpu.rs, pixels.rs, native.rs, wgc.rs, mf.rs, hdr.rs, clock.rs, display.rs, display/topology.rs. Round-1 items already applied (A1–A15) are not repeated. Round-1 A16 (long functions) returns here as A1–A4, now with concrete types for each split.

Counts: high 0, medium 7 (A1–A5, A8, A13), low 11. Every finding is `none` (refactor only).

1. **A1** · medium · Architecture
   - **file:line:** video.rs:83-457 (the frames merge owner: `Mark`, `Lane`, `MergeSignals`, `Frames`, `Watch`, `MergeLane`, `Constraint`, `Merge`); video.rs:1753-1913 (`Verifying`, `Closed`, `Segment`, `withdraw_revoked`).
   - **problem:** video.rs mixes three unrelated concepts in 1913 lines:
     - the §6.5 cross-monitor frames journal merge, which has its own thread;
     - the per-monitor slot and encoder state machine;
     - MP4 segment assembly and verification bookkeeping.
   - **fix:** turn `video.rs` into a `video/` module:
     - `video/frames.rs` takes lines 91-457 and exposes `Frames`, `Lane` and `Mark` as `pub(super)`;
     - `video/segment.rs` takes `Segment`, `Closed`, `Verifying`, `withdraw_revoked`, `within_media_prefix` and `ceil_us`;
     - `video.rs` keeps `run`, `record`, `Video` and `Selection`.
   - **size:** M (code moves only) · **behaviour:** none

2. **A2** · medium · Architecture (missing type)
   - **file:line:** video.rs:812-819, 826-830, 844-846, 868-870, 881-888, 890, 927; fields at video.rs:660-675.
   - **problem:** The state that survives pauses and format changes exists twice: as locals in `record` and as fields of `Video`. The fields are `published_through`, `staged`, `staging_bound`, `last_receipt`, `paused_slots`, `verifying`, `next`, `segment_number` and `required_finalization_failure`. The state moves between the two copies through 6 `mem::take` calls and a `carry` closure.
     - Adding a field and forgetting one of the copies silently resets that state at a pause.
     - The duplication is the main reason `Video` has 45 fields.
   - **fix:**
     - Add `struct Publication { published_through, staged, staging_bound, last_receipt, paused_slots, verifying }`. `record` owns it, and `Video` holds `&'a mut Publication`.
     - Move `stage_slot`, `resolve`, `resolve_all`, `publish_resolved` and `drain_lane` onto `Publication`.
     - Add `struct Progress { next, segment_number, required_finalization_failure }`.
     - Delete `carry` and every `mem::take`.
   - **size:** M · **behaviour:** none

3. **A3** · medium · Architecture (god function, long parameter lists)
   - **file:line:** monitor.rs:209-690, with an immediately invoked closure at 256-675 that captures more than 15 mutable locals; helpers with 11–12 parameters at monitor.rs:737-742, 769-774, 816-820, 866-871 and 938-942.
   - **problem:** `capture` is about 480 lines. No part of it can move into its own function without threading more than 10 `&mut` locals.
     - `Resources::publish`, `release_staged`, `submit_video`, `TimelineWait::release` and `deliver_converted` all take the same group: `video, timeline, converter, pending_video, waiting, size, now, counters`.
     - The format-reply block (316-394) nests 7 levels deep.
   - **fix:**
     - Add `struct VideoFeed { target, converter, pending: Option<PendingConversion>, waiting }`, with methods `submit`, `deliver`, `discard` and `release_waiting`.
     - Add a `PendingConversion` struct. It replaces the `.2` tuple index at monitor.rs:60, 509 and 886.
     - Make a `CaptureOwner` struct from the locals, with methods `on_pause`, `on_format_reply` (316-394), `request_format` (395-437), `on_geometry_change` (453-475), `admit_frame` (538-620) and `finish`.
     - `admit_frame` returns `Result<Admitted, Reject>`. This merges the three copies of "log, count, close, continue".
   - **size:** L · **behaviour:** none

4. **A4** · medium · KISS / DRY
   - **file:line:** encode.rs:71-240 (called from video.rs:1021 and encoder.rs:287); duplicate branches at encode.rs:133-157 and 161-181; a 5-tuple at encode.rs:110, 114, 117, 141, 167 and 218.
   - **problem:**
     - `encode::record` takes `&mut S` plus four closures, each of which receives `&mut S`. This is a hand-written trait.
     - The async and sync branches each repeat the output-retire step and the prefix-drain restart.
     - The pending picture is a tuple read as `.4`.
   - **fix:**
     - Add `trait EncodeOwner { observe_admission, next_frame, prefix_drained, output }`, implemented for `Video` and for the self-check `Check`.
     - Extract `retire()` and `restart_after_prefix()`.
     - Add `struct Picture { input, pts, duration, force_idr }`, used as `Next::Frame(Picture)` and `pending: Option<(Picture, Instant)>`.
   - **size:** M · **behaviour:** none

5. **A5** · medium · DRY / missing abstraction
   - **file:line:** three copies of the same `match worker.admit()?` at video.rs:1074-1086, 1201-1217 and 1721-1736; open-only checks at video.rs:772 and 957.
   - **problem:** Three places match `Admission` against `self.opening` the same way:
     - the `Open` arm is guarded on the boundary;
     - the `Closed` arm sets `self.cutoff`;
     - a catch-all arm returns an error.

     A rule change has to be made three times.
   - **fix:**
     - Add `enum EpochAdmission { Open(SessionTimeUs), Closed(AdmissionBoundary) }`.
     - Add `Video::admission() -> Result<EpochAdmission>`. It records the cutoff and returns the single error.
     - Add `still_open(worker, opening)` for video.rs:772 and 957.
   - **size:** S · **behaviour:** none

6. **A6** · low · Architecture (repeated parameter group)
   - **file:line:** video.rs:720-745 (`lose_video` and `release_encoder` both take `worker, id, summary, unavailable_video, reports`); video.rs:939-946 (`FormatOwner` repeats three of them); wrappers at video.rs:1060 and 1688.
   - **fix:**
     - Add `struct VideoLoss<'a> { worker, id, unavailable, reports }` with `lose(summary, error)` and `release(encoder, summary)`.
     - `Video` and `FormatOwner` each hold one `VideoLoss`.
     - The wrappers `flag_failure` and `release` go away.
   - **size:** S · **behaviour:** none

7. **A7** · low · KISS
   - **file:line:** video.rs:953-1013; `change.reply.try_send` appears 7 times (958, 965, 976, 982, 993, 1001, 1011).
   - **problem:** Seven early returns each send their own reply. A new branch can forget its reply, and the capture owner then waits until the next topology tick.
   - **fix:** Split out `decide(..) -> Result<(FormatReply, Option<Geometry>)>`. The caller sends the reply exactly once.
   - **size:** S · **behaviour:** none

8. **A8** · medium · DRY / missing type
   - **file:line:**
     - display.rs:29-45 (`Monitor`) and topology.rs:19-46 (`Output`, which copies 11 of the 13 fields);
     - `MonitorCapture` filled by hand twice, at monitor.rs:992-1016 and 455-462, each time splitting `refresh` into numerator and denominator;
     - the "same item" test at monitor.rs:332, monitor.rs:414 and inside `same_format`;
     - "resolve and compare with the target" at monitor.rs:326 and video.rs:970-975. Line 973 calls `GetDesc1` again, although `from_native` already read the LUID.
   - **problem:** One set of display properties is declared three times and copied by hand in four places. Adding a display property means editing four places.
   - **fix:**
     - Add `struct DisplayProperties`, embedded in both `Monitor` and `Output`.
     - Add `MonitorCapture::apply(&DisplayProperties)`. The serialized names stay the same.
     - Add `Output::same_item()` and `display::resolve_matching(key, &Output)`.
     - Add an `AdapterId` newtype with `From<LUID>`. It replaces the `(u32, i32)` tuple, `same_adapter` (monitor.rs:988) and the second `GetDesc1` call.
   - **size:** M · **behaviour:** none

9. **A9** · low · Architecture (missing type)
   - **file:line:** "encoder format plus hardware/software" is passed as `(VideoFormat, bool)` or as sibling fields at about 10 sites: monitor.rs:79 and 97-99; video.rs:685, 1006, 1011 and 1308; encoder.rs:53-56, 100 and 168; gpu.rs:85-88.
   - **fix:** Add `#[derive(Clone, Copy)] struct Encoding { format, software }`.
   - **size:** M (mechanical) · **behaviour:** none

10. **A10** · low · DRY
    - **file:line:** wgc.rs:45-65 and wgc.rs:157-174.
    - **problem:** The FP16-then-BGRA8 fallback rule (VID-8) is written twice, almost line for line.
    - **fix:** Add `with_color_fallback(hdr, white_known, attempt) -> (T, ColorMode)`. A `ColorMode` struct replaces the tuple `(Capture, CaptureColor, Option<i32>)`.
    - **size:** S · **behaviour:** none

11. **A11** · low · Idiomatic (state encoded in bits)
    - **file:line:** video.rs:103-133 (encode), 361-371 and 414-421 (two hand-written decodes, each with different guards).
    - **fix:** Add `enum MarkTag { None, Open(u64), Closed(u64), Exited }` with `encode` and `decode`, plus `Mark::tag()`.
    - **size:** S · **behaviour:** none

12. **A12** · low · Architecture (leaky abstraction)
    - **file:line:** mp4.rs:35-40, 132, 201, 229; video.rs:1758 and 1763-1776.
    - **problem:** The claim protocol between the worker and the verifier is a raw `Arc<AtomicU8>` with public `u8` constants.
      - video.rs runs its own `compare_exchange` on it.
      - `Verifying.abandoned` duplicates the claim's state.
    - **fix:** Add `mp4::Claim` with `try_abandon`, `try_publish` and `is_abandoned`, and make the constants private.
    - **size:** S · **behaviour:** none

13. **A13** · medium · DRY across the workspace / inconsistent conventions
    - **file:line:**
      - A `(QpcTick, suspend sequence)` tuple at video.rs:224 (`Watch.since`), video.rs:1760 (`Verifying.handoff`) and still_rows.rs:429. The same pair already exists as a named struct, `ProgressMark`, at input/receiver.rs:48.
      - "Restart the deadline when the suspend sequence changes" is written at video.rs:1069, video.rs:1363-1369, monitor.rs:507-509 and elevated/selector.rs:579.
    - **problem:** Two idioms for "a wait that excludes sleep" exist side by side: one uses `awake_elapsed`, the other restarts an `Instant` deadline. Every site writes its own version.
    - **fix:**
      - Add `AwakeMark { qpc, suspends }` and `SuspendMarks::since(mark, ..)` in session/windows/environment.rs.
      - Add `SuspendWatch(u64)` with `slept()` for the deadline-restart sites.
      - Keep each site's current semantics. Unifying all sites on `awake_elapsed` would be a `change`.
    - **size:** M · **behaviour:** none

14. **A14** · low · Idiomatic (state machine in fields)
    - **file:line:** slots.rs:185-229 and 261-291; the resolved/unresolved test is written by hand at video.rs:1608, 1628, 1633 and 1704.
    - **problem:**
      - A row is Pending, Encoded or Missing, but this is stored as `encoded: bool` plus `reason: Option<Missing>`.
      - `withdraw` clears 15 fields by hand. A new source field that is not added there would leak into a withdrawn row.
    - **fix:**
      - Add `Row::is_resolved()` and `Row::is_unresolved()`.
      - Group the fields as `#[serde(flatten)] Option<RowSource>` and `Option<RowEncoding>`.
      - The JSON stays the same, because every field is skip-if-none and `preserve_order` is not enabled. Confirm this with a golden-row test first.
    - **size:** M · **behaviour:** none (the serialized output must stay identical)

15. **A15** · low · DRY (small helpers)
    - **`create_nv12_texture`:** gpu.rs:260-279 differs from `pixels::texture` (pixels.rs:126) only in `MiscFlags`. Add a `misc` parameter to `pixels::texture`.
    - **Pool idiom:** `Arc::strong_count == 1` appears at gpu.rs:242 and native.rs:75. A `LeasePool<T>` is optional.
    - **MF packing:** the Media Foundation `UINT64` high/low packing is written four times, at encoder.rs:308-309 and 338-340 and at mp4.rs:275. Add a helper `mf::pack(hi, lo)`.
    - **size:** S · **behaviour:** none

16. **A16** · low · Idiomatic (`as` casts)
    - **file:line:** video.rs:480 and 986 (i32 to u32); gpu.rs:125-127 and 152-158; encoder.rs:261; monitor.rs:796.
    - **problem:** Sizes switch between `SizeInt32` and `[u32; 2]` with unchecked `as` casts at each boundary.
    - **fix:** Make `Converter::submit` take `[u32; 2]`. Convert from `SizeInt32` once, with `u32::try_from`.
    - **size:** S · **behaviour:** none

17. **A17** · low · Idiomatic
    - **file:line:** readback.rs:200-242 and 282.
    - **problem:**
      - "Send the rejection, then return" is written three times.
      - `cpu_bytes: Option<Bytes>` needs an `.expect`, because it is taken separately from `work`.
    - **fix:**
      - Send the rejection once, from the caller of an inner `prepare`.
      - Store `work: Option<(Work, Bytes)>`.
    - **size:** S · **behaviour:** none

18. **A18** · low · KISS / dead weight
    - **file:line and fix:**
      - monitor.rs:89: `enum Exit` has a single variant. Replace it with `Option<AdmissionBoundary>`.
      - topology.rs:134-143: `geometry()` duplicates `capture_geometry()`, and both return `Result<_, ()>`.
      - mp4.rs:134 and 190: "revoked, then `remove_part`, then `Ok(None)`" is written twice.
      - video.rs:1871 uses `.expect`, while video.rs:1527 handles the same invariant with `ok_or_else`. Use the error path in both.
      - video.rs:1468-1486: channel state is turned into `Err(bool)`. Match `TryRecvError` directly.
      - h264.rs:14-15: a `Vec` used with find-or-push. Use a `BTreeMap`.
      - mf.rs:181, 202 and 228: `windows::core::Interface` is imported again although line 9 already imports it.
    - **size:** S · **behaviour:** none

## Assessment
The pipeline is functionally careful. The low-level modules are clean and well commented: the h264 parser, the slots `Timeline`/`Evidence`, the mf loader, clock, hdr, the readback budget and the merge algorithm itself. They need no structural work.

The debt sits in the two orchestration files, video.rs and monitor.rs:
- video.rs mixes three concepts and carries its cross-epoch state through a `carry` closure (A1, A2).
- `monitor::capture` is one closure over more than 15 locals, with a parameter group repeated across five helpers (A3).
- Small abstractions are missing in several places: epoch admission (A5), display properties (A8), encoding (A9) and awake-time marks (A13).

The rest is low-cost polish.
