# screen_fix · Intercall Recorder M1

The Windows desktop recorder collects approved on-screen activity, input and context into a local attempt. M1 is the capture core. The workspace contains the desktop executable and its separate input helper.

## Entry points

| Package | Entry point | Responsibility |
| --- | --- | --- |
| `screen-fix` | `ui/src/main.rs`, no arguments | Opens the desktop recorder. `ui/src/app.rs` submits commands and renders session status. |
| `screen-fix-input-helper` | `core/capture/input/src/main.rs` | The recorder launches the helper with its own restricted IPC handles. Launching the helper directly does not start a recording. |
| `screen-fix-core` | `core/src/platform/mod.rs` | Owns the production session entry, package API, and native capture, encoding, context, and writing. |
| `screen-fix-protocol` | `protocol/src/lib.rs` | Defines portable time, evidence, identities, and bounded IPC records. |

For a future Windows build, build both workspace binaries together:

```powershell
cargo build --release -p screen-fix -p screen-fix-input-helper
```

Keep `screen-fix-input-helper.exe` beside `screen-fix.exe`. The workspace sets the desktop as its default member. The helper manifest and deployment metadata specify an `asInvoker`, non-UIAccess binary for the same release, installed under `Program Files\screen_fix` and Authenticode-signed for release packaging. This source delivery contains no release binaries, installer, signatures, or `Cargo.lock`. Unlocked dependency resolution can change across builds. The recorder backend and historical exporter are Windows-only.

## Recording an attempt

1. Open the application to be recorded and select its executable under **Approve an executable…**. Use the approved application in the foreground when the countdown ends. The M1 stub selects one executable in the desktop UI and gates public evidence by foreground and receipt. An empty approval list grants no approval.
2. Choose the output folder, displays and capture pace. Clipboard and pen-HID collection are opt-in. Keep each selected window on a selected display when its screen evidence is required.
3. Open **Recorder hotkeys** if a default cannot register. Start, Pause/Resume, Checkpoint request and Stop default to Ctrl+Alt+F9/F10/F11/F12. Windows reserves F12 for debugger use. Choose an unused function key from the actual F1–F24 choices, for example F8 if it is free, then wait for registration or use **Retry hotkey registration**. Start requires successful registration of all four bindings. The hotkey for a checkpoint creates a request. A still tied to a save needs successful-save evidence through the trusted fixture API in M1. Stop does not initiate a save.
4. Click **Start** or use its registered chord, then switch to the approved app. Startup probes Windows, executable and window eligibility, disk space, selected displays, each Media Foundation encoder, and helper registration. Use the desktop controls or registered chords for pause, resume and Stop. The result shows the attempt location, and failures include a local report path when a report could be saved.

Stop allows up to 10 seconds for normal finalization. If the deadline expires before the owner is authorized to publish the complete manifest, the recorder reports a timeout and the attempt stays incomplete. If authorization was obtained in time, the recorder shows **Final publication pending** while Windows finishes the atomic replacement. The same attempt stays active and holds the recording lease. The final result reflects the Windows response, with a native error in diagnostics if publication fails. Windows shutdown and suspend retain their shorter waits.

The desktop passes a frozen config to the controller thread in `core/src/session/windows.rs`. The coordinator in `core/src/platform/windows.rs` starts the package, launches monitor owners, and starts the input helper through `input.rs` and `input_helper.rs`. The input owner routes validated receipts through matching, held state, context, UIA and the text publication gate. The capture owner holds native, cursor-free pixels and sends independently paced video samples to its monitor encoder. Early still requests also reach the capture owner through bounded lanes. Approved image results go from retained native frames through staging/readback and the PNG owner to the still journal. Optional custom cursor PNGs have their own bounded asset owner.

Executable version resources are an optional snapshot of approved files read by the startup coordinator before capture starts. Window records label this provenance as `approved_file_at_startup`; it does not prove the version of a loaded process image. During capture, alternative executable path spellings are resolved by one separate owner with 64 bounded pending requests. Unknown approval remains private. An asynchronous positive result admits only receipts at or after its QPC confirmation boundary. Resolution failure or a pending wait exceeding two seconds is diagnosed; no replacement owner is started while the old one still holds a native call. Executable selection uses the existing setup worker, so path resolution does not block the desktop UI.

## Output and inspection

The selected output directory contains `attempts/attempt_<id>/`:

```text
manifest.json
raw/input.jsonl
raw/context.jsonl
raw/frames.jsonl
raw/stills.jsonl
raw/video/m<monitor>/seg_<segment>.mp4
raw/images/<digest>.png
raw/cursors/<digest>.png
```

| Evidence | How to join it |
| --- | --- |
| Clock | `manifest.clock` records the QPC origin and frequency, measured UTC FILETIME brackets labelled by admission epoch, and both the actual Stop and last admitted cutoff. UTC is sampled at Start, at intervals of up to 60 seconds while the attempt is active, and at Stop, including while paused. When stopped from pause, the cutoff remains the time admission closed. Interpolate UTC within the same continuous interval. Context records describe pauses and discontinuities. |
| Selected displays and actual modes | `selected_displays` is the request. `monitor_capture` holds the last known mode and aggregate counters. `monitor_epochs` records activated capture and geometry versions. Context records describe missing/reconfiguring displays and topology. A segment lists its own resolution, effective FPS, hardware/software selection, configured bitrates and opening capture/geometry identities. |
| Event timing | Journal `recv_t_us` orders rows by logical receipt, with `seq` breaking ties. `source_t_us` is null when the provider has no session-clock timestamp. Raw Input, hook and WinEvent ticks carry `tick_ms` and `tick_uncertainty_us`, with `late` null when unclassifiable or based on an injected hook. Context rows triggered by a matching WinEvent carry `win_event_recv_t_us` for the native callback receipt used by the lateness check. Their `recv_t_us` describes the context poll. UIA completion and cursor observation retain separate times. |
| Profile and input | The manifest records stub revision, approved identity count, requested options, helper registration, bounded observed devices/settings and unavailable evidence. Device descriptors are observed across the attempt, including devices that may have been removed. Missing calibration is explicit. |
| Key transitions | Approved native `key_down` and `key_up` rows retain normalized `vk`, `scan` and `key`, with separate `raw_input` and `keyboard_hook` witnesses. A missing witness is `null`. `VK_PACKET` scan fields are `null` because the units belong to `text_commit`. `modifiers` and `modifiers_known` describe the state after the transition. From bit 0 to bit 7 the sides are left/right Shift, Ctrl, Alt and Win. A known bit with a clear held bit means released. A clear known bit means unknown. Synthetic key edges describe a boundary, with both witnesses `null` and all modifier sides unknown. |
| Hung applications | In `context.jsonl`, pair `app_hung` boundaries by `process`, `window`, and `thread_id`. A verified Ghost continues the original window’s interval. An unresolved Ghost produces an unattributed `app_hung_ghost` marker with `association: "unknown"`. `app_hung_coverage` indicates limited observation. Process-scoped `app_busy` rows with `scope: "process_hung"` report the hung contribution to busy state. Pause and Stop end observation without establishing recovery. Settled images and hung evidence cannot establish operation completion. |
| Encoded frames | Join `monitor` and `segment` to the MP4; the index carries `frame_in_segment`, decoded-local `pts_100ns` and duration. Session slot time, native capture source time, capture receipt, and journal receipt are separate. On repetition, the native frame identity is reused. A skipped slot has an explicit reason and no encoded sample reference. |
| Native stills and cursor assets | In `stills.jsonl`, `event_ref` locates the causal input journal row by stream and sequence. `frame.id` identifies the actual source even if it has no corresponding video sample. Successful rows point to the published `file` URI and can share the same PNG. `geometry`, admission and save fields preserve selection provenance. In `context.jsonl`, an available `cursor_asset` references the optional cursor PNG and XOR mask independently of screen pixels. |
| Controls and loss | `context.jsonl` contains recorder controls, checkpoint request IDs, pause/resume/Stop evidence and input blind intervals. Every journal has its own sequence and receipt order. Gap records preserve known queue-loss counts and bounds; other loss evidence can carry unknown counts. |
| Measurements | Closed journals report rows, loss, reserved entry/byte peaks and flush counts and latency. Capture reports native allocation and pressure counters. Video reports slot outcomes and repetitions. PNG records pressure, deduplication, and maximum queue and processing duration. These are producer measurements and buffer accounting, not end-to-end acceptance measurements. |

`package::metadata::PackageReader::open` loads the manifest. `artifact_for_uri` resolves canonical attempt-relative paths, and `open_artifact` checks identity, kind/inventory and file shape. The callable `PackageReader::validate_v5_artifacts` is a read-only API for later package inspection. It streams listed JSONL, checks sequence/receipt/gap reconciliation, validates segment index/extent and checks still/cursor references, PNG headers and recorded still dimensions. It does not run on the desktop or Stop path. During recording the segment owner verifies decoded timing and dimensions before publishing each MP4. Stop joins admitted producer work and reconciles inventory and closed stream summaries before atomic manifest publication. It does not reopen and decode the entire completed attempt.

A `complete` v5 attempt has certified required closure, including usable video from every selected monitor. It can contain accounted losses or degraded encoding modes. Cancellation, approval suppression, unverified required streams and finalization failures can leave an `incomplete` attempt. Pending and part files are not published artifact references. A timeout before authorization can leave the initial incomplete manifest and physical files from the closed prefix.

The explicit Windows command `screen-fix --export <completed-session-directory>` retains historical v3 and compatible single-video v4 behavior. It writes a new timestamped export directory. There is no video-to-PNG export path for v5. Legacy single-file recording and automatic post-Stop export are superseded by v5.

## Current limits

- Pen pressure is recorded only when observed through the selected native HID path. Vendor tablet mode and per-application settings are unknown/unavailable. No Wintab context is opened. The touchpad SPI and contact-report paths are present. Per-device Precision Touchpad Feature Type is explicitly unavailable because blocking Feature IO on the Raw Input owner has no deadline. No MIDI path is required by M1.
- The 300 MiB per 4K monitor figure is a measurement target. Native texture and queue counters do not include the complete GPU/driver footprint.
- M2–M6 cover task/file workflows, production application adapters, full privacy/review, delivery views, upload and server services. The stub and fixture routes here do not implement those services.

The current source delivery and remediation outcome are recorded in [SCREEN_FIX_M1_REMEDIATION_STATE.md](SCREEN_FIX_M1_REMEDIATION_STATE.md). The original M1 implementation and validation record is in [SCREEN_FIX_M1_STATE.md](SCREEN_FIX_M1_STATE.md). Requirements and scope amendments are in [doc/](doc/), including the baseline technical specification, M1 scope, architecture and algorithms addenda, and the development plan. Compilation, formatter execution, application launch, tests, benchmarks, Windows/runtime verification and hardware acceptance were not performed for this source delivery.
