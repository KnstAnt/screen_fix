# screen_fix MVP specification and roadmap

The project goal is recorded in [target.txt](target.txt). Direct user requirements govern the contract. This document defines the MVP contract and acceptance criteria. See [architecture](SCREEN_FIX_ARCHITECTURE.md) for implementation status and open verification items.

## 1. Goal and warranty scope

The Rust application collects desktop demonstrations for AI training on Windows 10 version 2004 (build 19041) and later, including Windows 11. In an explicitly started session it records a selected display continuously, captures input and available context separately, and extracts screen states before and after events. Capture is local, with visible recording status and normal Stop.

The guarantee covers normal operation. Software and hardware failures and other abnormal conditions are outside the recovery contract. Data loss during a failure is acceptable. The MVP has no obligation to recover interrupted sessions or guarantee the integrity of an unfinished file. On a detected failure, end the affected operation, show a clear explanation, and create a local diagnostic report for the user to send manually. Do not retry, recover, or resume automatically. Reporting depends on the process still being able to report. Publish successful output only after normal completion.

Reports include time, app and OS versions, stage, available profile information, and technical error code. Exclude captured video, input, clipboard contents, and window titles. If the report cannot be saved, show available diagnostics for copying.

Normal operation assumes an unlocked interactive session, an available selected display, a supported GPU and driver, working local storage, and agreed bounds for resolution, cadence, input rate, and duration. Secure desktops, protected surfaces, sleep, display disconnection, and resource exhaustion outside the profile are excluded. Ordinary app switches, typing, scrolling, sustained dragging, and workloads inside the profile are normal scenarios. Queue overflow inside the accepted profile is a defect. A detected overflow disqualifies the session from successful publication.

Completeness covers all input delivered by the selected sources in the session interval and frames selected by the agreed profile. It does not extend to internal application commands, every compositor update, or activity outside the selected display. The timeline covers idle intervals through sample duration or technical repeats with the original observation time retained.

## 2. P0 MVP requirements

| Area | Contract |
|---|---|
| Session | Select one display and a local directory, then explicitly Start and Stop. Show recording status. Switching applications keeps the selected display as the source. |
| Video | Continuous H.264. The initial test candidate is SDR 1920×1080 at nominal 30 fps. Confirm limits and small-text readability through measurement. |
| Input | Capture key and mouse button presses and releases, movement, and scrolling. Preserve raw codes, flags, movement, and available desktop position. Raw key codes do not establish typed text. Preserve movement and repeated key events in the source log. |
| Context | Capture initial foreground window and switches, with available process name and title. Clipboard notifications and available Unicode text are opt-in. Record why values are missing. A clipboard change does not establish a paste. |
| Output | Provide a local package with video, event log, manifest, and before/after exports. Keep source files for repeat export. Drag exports use interval boundaries and the intervening video. |

Capture runs during a visible, user-started session. The user selects the destination and controls sharing. Automatic transfer and default clipboard text capture are outside the proposed profile.

## 3. Architecture and dependencies

| Component | Owner and output | MVP implementation |
|---|---|---|
| Session | One coordinator starts sources, defines boundaries, and publishes state and the final manifest. | Rust with an egui/eframe desktop UI and a Windows-native folder picker. One process with separate threads across blocking I/O boundaries. |
| Video | Owns WGC, D3D11, conversion, encoder, video file, and saved frame timing. | WGC on the selected display, Cursor-free GPU conversion to NV12, pre-encode pixel fingerprinting, hardware H.264 through Media Foundation, and MP4 finalized at Stop. |
| Input and context | Owns Win32 registrations and emits observations to the log. | Background Raw Input, WinEvent foreground notifications, and opt-in clipboard notifications. Move potentially blocking enrichment outside the input pump and verify the source identity or version. With clipboard text enabled, the input journal writer may wait for the owning application. The UI and capture threads do not wait for that read during recording. Normal Stop waits for the journal to close. |
| Journal | One owner per file writes the event log and frame index. | Buffered, versioned JSONL. Package files are manifest.json, video.mp4, events.jsonl, and frames.jsonl. Keep exports separate. |
| Export | Reads closed packages and extracts images selected by timestamps. | Media Foundation decoding and PNG output through the `png` crate. Decode selected ranges sequentially and reuse images across events. |

Use the relevant `windows` features, `serde` and `serde_json`, and standard-library threads, buffering, and bounded channels. Keep COM and FFI in the Windows layer with explicit lifetime and ownership rules. Session logic, schemas, and matching use Rust values. MVP requires no FFmpeg, database, async runtime, background indexer, or cloud service.

Choose dependencies by maturity, integration and distribution cost, and clean and incremental build times. Prefer an appropriate mature Rust crate when adoption differs by only severalfold. Prefer an established, well-tested C or C++ implementation through a maintained Rust wrapper when the Rust alternative is rarely used and poorly validated. Account for transitive dependencies, native toolchains, and DLLs. If a database is needed, use `redb`. MVP uses MP4 and JSONL.

Verify the hardware path before acceptance. Sink Writer hardware MFT support alone does not guarantee hardware encoding on Windows 10. Prefer explicit hardware H.264 MFT selection, D3D11 validation, and muxing the compressed stream. Sink Writer encoding is acceptable if the chosen hardware MFT is verifiable on the target matrix. A Windows 11 25H2-only attribute cannot serve as the common Windows 10/11 contract. Refuse Start with a reason if the required hardware profile is unavailable. Software encoding may be evaluated as a separate profile later.

## 4. Timing and data

- Use a common monotonic QPC-based timeline. WGC provides compositor time through `SystemRelativeTime`. Timestamp input on receipt and record timestamp provenance. Preserve intra-stream order when reading batches without inventing hardware timestamps. Window and clipboard observations have their own times. Calendar time belongs in the manifest.
- Each event includes schema version, stream sequence, time, type, and payload. Store physical virtual-desktop cursor coordinates with provenance. A position read during Raw Input processing may lag the action. Store original frame time, MP4 PTS, frame size, and the display-to-video transform. Sequence resolves equal timestamps within a stream. Equal times do not establish cross-stream order.
- For a discrete event, choose the latest real observation strictly before it and the earliest strictly after it. For paired button actions, use the content-aware policy below. Repeats do not count as new observations. Give a reason if a side is missing. If an application redraws late, retain the video for further annotation. Exports do not invent frames outside the session.
- Export key edges, scroll, foreground switches, and clipboard changes as discrete events. Export button edges as intervals. Keep pointer movement in the source log. A drag may be derived from movement while a button is held. Window metadata describes observed focus and does not identify the actual recipient of every input.

### Content-aware PNG export

Disable cursor capture in WGC before opening a recording. Windows 10 version 2004 or later is required for this property. Preserve pointer coordinates and button edges in the raw event log. Compute BLAKE3 from the visible NV12 bytes before H.264 encoding, ignoring row padding. Identical fingerprints share a PNG even if observation times differ. Keep observation PTS and image PTS separate. This compares exact pixels in the output profile and does not attempt perceptual matching.

Pair presses and releases by device and button. Export the nearest real observation strictly before the press as the baseline when available. Include distinct observations from the half-open hold interval and the first real observation strictly after release only if their fingerprints differ from the baseline. If the baseline is unavailable, report the missing comparison. If the release is unavailable, omit the after side. Write button intervals without confirmed movement to `clicks.jsonl` and confirmed drags to `drags.jsonl`. Preserve incomplete-boundary markers. Button edges do not generate separate PNGs in the discrete-event export. Keyboard, wheel, focus, and clipboard events retain their discrete mappings.

Version 3 sessions persist fingerprints and support this export policy. Older sessions may include the cursor and lack pre-encode fingerprints. The content-aware exporter rejects them with a diagnostic.

## 5. Normal completion and acceptance

Initialize required sources before opening the session. At Stop close admission, drain accepted events and frames, finalize encoding and the container, flush journals, and publish the final manifest. Export reads a closed session. Bound queues and buffers using measurements. Video must not block the input pump. Keep OCR, action recognition, synchronous per-event logging, and video decoding out of capture.

Before acceptance, agree on the Windows/GPU matrix, maximum session length, allowable gaps on changing screens, timestamp accuracy, text readability, CPU/GPU/RAM/VRAM budgets, and impact on app responsiveness. Compare the same workflow with recording enabled and disabled. Exercise Blender and one target Adobe or CAD application, prolonged recording, heavy input, fast dragging, focus switches, and normal Stop. Check event counts against a known sequence, queue depths, timing, decodability, and unexplained loss inside the accepted profile. Compatibility and budgets require measurement.

## 6. P1 and P2 after MVP

| Priority | Feature | Condition |
|---|---|---|
| P1 | Multiple displays, single-window capture, and live configuration changes | Verify timing, coordinates, and multi-stream costs. |
| P1 | More quality profiles, HDR, DXGI capture, and additional encoders | Driven by compatibility, readability, and performance data. |
| P1 | App exclusions, pause, and more control over sensitive data | Driven by dataset collection and distribution requirements. |
| P1 | `redb` session search, independent MKV segments, or fragmented MP4 | For search, partial export, or long recordings. Recovery requires a separate contract change. |
| P2 | OCR, UI Automation, audio, action labels, and Blender/CAD/Adobe integrations | Driven by dataset requirements and measured cost. Keep semantic annotations separate from observations. |
