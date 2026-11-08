# screen_fix MVP architecture

The MVP specification governs this design. The user starts capture on a selected display, works in ordinary applications, stops, and receives local video, raw input, context, and frame exports. Minimum OS is Windows 10 version 1903 (build 18362), including later Windows releases and Windows 11. The guarantee covers normal operation. Detected failures follow the specification's reporting contract.

## Implementation status

This design records both the intended contract and the current source-level implementation. The packaged revision has not been built or exercised on Windows.

- The egui/eframe UI selects an attached display and an existing destination folder. Browse opens the Windows folder picker through `rfd`. Manual path entry is also supported.
- The recording worker owns WGC, D3D11 conversion, the hardware encoder, the MP4 writer, and frame indexing. A separate Win32 input worker feeds a separate journal writer through a 4,096-event queue. The optional clipboard read runs in that writer. Delayed rendering can extend Stop or, if the queue fills during recording, fail the session with diagnostics.
- Start, admission, and Stop are ordered under a mutex using `Instant`. The manifest stores a QPC anchor, frequency, uncertainty, video start offset, and Stop offset. The current format does not persist a separate gate operation sequence. Event and frame records have their own identifiers.
- The recorder targets 30 fps and selects the newer queued frame when two are available. PTS is derived from monotonic admission time. The prior frame is held to compute duration, and missed slots are skipped. D3D11 conversion uses up to 16 NV12 textures, tracked samples, and a fence wait before releasing the WGC frame.
- After Stop, the recorder checks decoded frame count, ordering, PTS, duration, and end time against the submitted index. The timing tolerance is 1 ms. It publishes the source manifest before exporting PNGs. Repeat export reads manifest version 2 and writes a new derived directory.
- The current diagnostic file contains the application version, platform name, stage, and available technical details. Explicit timestamp, OS build, and profile fields remain gaps against the specification. Windows-provided descriptions can use the OS language.
- Foreground titles and process names are observed when the notification is handled. Clipboard sequence numbers describe the value observed when the notice is read, so multiple queued notices can refer to the same clipboard version.

## 1. Ownership and execution

| Owner | State and resources | Output |
|---|---|---|
| Session coordinator | Start/Stop, UI state, session boundaries, directory, and manifest | Admission boundaries and final status |
| Win32 message thread | Raw Input, focus and clipboard notifications, observations and sequence numbers | Input and context events |
| Video owner | WGC, D3D11, texture pool, encoder, and MP4 | Video and saved frame index |
| Input journal writer | Event file, buffers, counter, and optional clipboard text read | Closed events.jsonl and completion acknowledgment |
| Export | Closed package reader, decoder, and image writer | Event-to-image mapping and PNGs |

One process. Control, Win32 input, video, and log writes execute independently. Send values through bounded channels. The video worker writes the frame index directly. The input journal uses an independent bounded queue. Queue overflow reporting must not depend on free space in the affected queue. COM objects, GPU surfaces, and file descriptors remain with their owners. Move blocking enrichment outside the input pump. Report missing values when attribution to the source observation cannot be established.

Use `windows` for platform implementation and `serde` and `serde_json` for the package format. Use standard-library threads, queues, and buffers. Keep system APIs inside Windows modules. Session and matching logic operate on Rust values. Use `redb` if a session index is added. Follow the specification's rules for dependency maturity, integration, and build cost.

## 2. Session protocol

1. **Prepare.** Verify the monitor, directory, source registrations, and hardware video profile. Create a unique session directory. Prepare sources and writers. Show preparation status, then recording status. Initialize WGC during preparation and start capture after admission opens so the first frame of a static screen can enter the session. Register focus notifications during preparation and read the initial focus after opening. Include time to first frame in profile verification.
2. **Open.** The coordinator uses a small shared gate with Prepared, Open, and Closed states. State transitions and observation admission are serialized. The critical section reads QPC and decides admission. GPU work, I/O, serialization, and queue waits take place outside it. Each admission applies to one observation in one session.
3. **Admit.** The interval is `[start, stop)` on the logical key (QPC, gate operation order). Preserve boundary and admission keys. Video time is the numeric projection. Read admission time inside the gate. Timestamp input and context when observed by the process. Retain WGC compositor time separately. Exclude frames with compositor time before Start and data delivered after closing, even if the OS generated it earlier. Admit before expensive processing and before queueing. Overflow of a required queue after admission makes the session incomplete.
4. **Stop and drain.** Close admission and stop sources. Each source finishes admitted operations, closes its channel, and reports its count or error. Drain queues, reconcile counts, and close journals. Drain the encoder and finalize MP4. Publish the source manifest after successful completion. Export runs afterward and has a separate outcome. Keep successfully recorded sources when export fails and distinguish recording status from dataset export status.

### Detected failure diagnostics

The owner returns a typed error with its stage and available Windows code. The coordinator ends the affected operation. For a recording failure it closes admission and releases resources without attempting continuation. Show a clear description and the path to a local report with time, app and OS versions, stage, available profile information, and error code. Exclude captured data. If the file cannot be written, show available diagnostics for copying. If the process terminates abruptly, the report and message may be unavailable. Report export failure separately from source recording status.

## 3. Video and timeline

- Path: selected display WGC → D3D11 → GPU NV12 → verified hardware H.264 MFT → MP4. Keep surfaces alive while the encoder uses them and reuse a bounded pool. Full-frame CPU reads are reserved for post-session export. Verify the hardware path for the Windows version, adapter, and driver. Reject Start with a reason if the required profile is unavailable.
- Convert compositor and observation times to integer 100 ns units relative to session QPC. Preserve raw time and its provenance. Check conversion range. Use sequence numbers for equal times within a stream. Equal cross-stream times do not establish causality.
- Nominal cadence defines selection intervals on the admission timeline. Process them in order and save the latest eligible observation within each interval, when available. Stop closes the final partial interval. Presentation of a new observation begins at or after its compositor time. If there is no new observation, extend or repeat the last saved frame while preserving its original time. Before the first frame the screen state is unknown. Clip final coverage at Stop. Measure selection delay and coverage on changing screens.
- The index distinguishes `observation_id` from encoded samples. Each sample records normalized PTS, duration, and its observation. Match decoded frames by presentation time, accounting for container timebase and reordering. Each indexed sample must match one decoded sample unambiguously. The initial measurement profile is one SDR display, 1080p, nominal 30 fps.

### Video pipeline contract

- Bind the selected adapter to the hardware MFT through LUID and the DXGI device manager. Verify BGRA/NV12/H.264 negotiation and GPU conversion. A negotiation failure rejects Start with diagnostics.
- Keep the WGC lease until GPU reading is complete or equivalent command ordering is guaranteed. The NV12 surface is separately owned. Return it to the pool only after confirmed release by the consumer. Submission or encoded output alone does not establish release.
- The async MFT accepts input on NeedInput and provides output on HaveOutput. At normal Stop, submit admitted samples, signal end of input, and drain through DrainComplete. Finalize MP4 after all output arrives. Detected failure follows the diagnostic path.
- Agree on SPS/PPS and a decodable stream start with the MP4 sink. The first profile may require an MFT with B-frames demonstrably disabled. If reordering is supported, preserve DTS separately from PTS and pass samples in decode order.
- A provisional index links observations to encoder samples. After closing MP4, reconcile timestamps, durations, start offset, and timescale with the actual container. Publish the manifest only after unambiguous reconciliation and index closure. The vertical slice also verifies decoded-frame correspondence.
- Stop before the first usable sample produces a distinct result, “Recording stopped before the first frame.” No successful package is published.


## 4. Data and export

| File | Contents |
|---|---|
| `manifest.json` | Format version, session ID, boundaries, profile, display geometry and physical-to-video transform, package contents, and stream counts |
| `events.jsonl` | Stream ID and sequence, observation time, typed input or context, and reasons for unavailable fields |
| `frames.jsonl` | Saved WGC observations and video sample mapping with PTS, duration, and repeat marker |
| `video.mp4` | Finalized video |
| `export/` | Event/image index, missing-side reasons, and PNGs reused by observation |

Preserve raw mouse motion, button, and scroll packets. Store observed physical pointer position separately from relative motion. Mark clicks outside the selected display as lacking a video coordinate. Window metadata describes observed focus. Log clipboard changes only when enabled. Store text only when the version matches the observation. If it changed, record a missing value.

For discrete events, choose the last saved real observation strictly before and the first strictly after the boundary. Use only observations with uniquely resolvable PTS. For drags use press and release. Mark gestures that started before capture or remained unfinished as incomplete and preserve raw input. Equal timestamps belong to neither strict side.

Expose presented screen state separately on sample intervals `[PTS, PTS + duration)` in session time, clipped at Stop. A static screen can have known presentation coverage with no strict after frame. Preserve this distinction. A repeat never replaces a strict observation. Window association and temporal proximity do not establish that the effect of an action is visible.

Match sorted events and observations by time. Deduplicate selected samples by observation and extract them during sequential decoding. Repeated references to one observation reuse its PNG. Repeat export does not modify the source package.

## 5. Acceptance and open checks

| Goal | Check |
|---|---|
| Completeness | A known sequence of keyboard, mouse, scroll, and focus events has no recorder-induced loss. Accepted counters and sequences match the closed log. |
| Boundaries | Exercise input and video near Start and Stop with delayed consumers. Drain admitted work, reject new work after closing, and publish success after all required files are closed. |
| Dataset | Exercise a static screen, delayed redraw, drag, equal timestamps, and missing sides at the boundaries. Resolve every exported reference to a decoded frame. |
| Performance | On Windows 10/11 with Blender and one target Adobe or CAD application, compare recording on and off. Measure app responsiveness, CPU/GPU, RAM/VRAM, disk writes, frame intervals, queue depth, and text readability. |

First verify the hardware vertical slice and PTS correspondence. Then connect capture, journals, and export. Use measurements to approve profile limits, queue sizes, input timing accuracy, and resource budgets. The design itself does not establish that these checks pass.
