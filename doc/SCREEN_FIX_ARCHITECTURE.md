# screen_fix capture architecture

The [specification](SCREEN_FIX_SPEC.md) describes the M1 capture scope. Use the [M1 architecture addendum](SCREEN_FIX_M1_ARCHITECTURE_ADDENDUM.md) for detailed ownership and extension boundaries, the [M1 algorithms](SCREEN_FIX_M1_ALGORITHMS_ADDENDUM.md) for capture and ordering, and the [elevated capture specification addendum](SCREEN_FIX_ELEVATED_COMPONENT_SPEC_ADDENDUM.md) for protected scope and attachment.

## 1. Process and owner boundaries

| Owner | State and responsibility |
| --- | --- |
| Recorder UI and controller | The egui/eframe UI submits commands and shows typed status. A dedicated controller owns the attempt, QPC origin, admission epochs, independent pause reasons and final outcome. |
| Standard input helper | Produces Raw Input, hook and supported device evidence. The Recorder launches it with its existing token through restricted inherited IPC with a parent-lifetime job. |
| Elevated collector | Runs on demand for an approved foreground requiring the collector, under one authenticated attachment. Owns scoped native input and context reads, its foreground/desktop witness and independent lifetime enforcement. Its control, input and context transport has separate bounded lanes. |
| Receiver and Context | The Recorder selects one source into shared matching, held-state and privacy logic. The Context owner manages visit and focus revisions, native read routing, geometry and evidence order. Clipboard content is owned by an opt-in worker within the Recorder. |
| Monitor capture and video owners | Each monitor has a WGC/D3D11 capture owner, native history and readback resources. The video owner processes paced samples and owns its encoder, segment finalization and index reconciliation. |
| Image and package owners | Bounded still and cursor asset work, PNG publication, individual journal writers and manifest publication run in the Recorder. Package inspection operates on closed artifacts. |

Use typed value messages across process and owner boundaries. Keep COM objects, GPU resources and writable files with their owners. Place platform calls in the Windows layers and shared time and wire records in the protocol crate. Queues are bounded and ownership of time-sensitive input does not depend on optional native reads or file I/O. The Recorder and collectors use common native observation code. The high process owns neither screen pixels nor recording storage.

## 2. Admission, transition and Stop

1. **Prepare.** Freeze the capture settings, check the Windows environment, storage, selected monitors, encoding profile and standard helper readiness. Identify the actual foreground at the countdown boundary. An approved foreground within the Recorder’s existing access uses the standard source without UAC. The asInvoker Recorder may already run at high integrity after a normal built-in Administrator launch.
2. **Attach when required.** Protected recording verifies the three matching role-bound signed images and installation protection. Both processes verify the live peer and approved target. The collector remains closed through grant preparation and required registration and held readiness. After the controller opens its epoch, exact activation and a fresh held sample precede collector evidence eligibility.
3. **Admit and publish.** Honor the current epoch, approved visit, original QPC and exclusive coverage. The Recorder certifies evidence only as far as its independently received input and context bounds and the safe cutoff allow. Frame, still and journal owners enforce the visit restriction pending publication. Checkpoint context and confirmed Save routing require current scope and original receipt within the same visit's proof.
4. **Pause or hand off.** A detected transition from the standard source to the collector clears frame approval and latches the old epoch closed. The controller preserves the attempt and clock. A configured idle collector can prepare only against the currently selected live foreground. Resume or a source switch requires fresh selected-source readiness and resynchronization. Retire old matching, context and media scope.
5. **Stop and drain.** Cancel unpublished source preparations and close active admission. If already paused, keep its earlier cutoff. Service outstanding source and writer work within the existing bounds. Collector cleanup acknowledgement and data certification advance separately. Only the supported retired prefix can drain. Preserve staging-loss and uncertified-tail evidence, finalize required streams and publish the manifest under the existing deadline and ownership rules.

A required source failure follows the same bounded closure path and retains its first substantive cause. The Recorder presents failure and any confirmed retained package, with a bounded local report in the selected output directory when available. Automatic repair and worker restart remain deferred.

## 3. Video, stills and timeline

- A monitor capture thread owns its GPU context, source-frame leases and retained history. Video pacing selects eligible samples on the common session clock and retains native capture time and receipt. Repeats reuse a source identity. Missing slots have explicit dispositions. A segment records its local PTS mapping, geometry and effective encoding mode.
- Startup selects the supported hardware path or the approved software path before admission. Encoding and muxing remain isolated from capture. Keep surfaces alive until the consumer releases them. Reconcile the closed segment with indexed timing before publication.
- The still path reserves native frames for bounded observation work, verifies geometry and admission, and reads back selected native pixels for PNG. The selection and manifest preserve evidence even when no corresponding video slot exists.
- Journal receipt, source QPC and encoder PTS describe separate events. Session time continues through pauses. The manifest records the QPC origin and frequency, UTC correlation brackets by admission epoch, and the actual Stop and last-admitted cutoff.

## 4. Package and verification

| Path | Contents |
| --- | --- |
| `manifest.json` | v5 attempt, clock and boundaries, selected monitors and geometry, inventories, stream summaries and optional selected-source evidence |
| `raw/input.jsonl`, `raw/context.jsonl` | Typed input and context, controls, blind/gap evidence and independent journal order |
| `raw/frames.jsonl`, `raw/video/m<monitor>/seg_<segment>.mp4` | Frame and slot index plus published segment media |
| `raw/stills.jsonl`, `raw/images/`, `raw/cursors/` | Logical still outcomes, durable evidence references and PNG assets |

The read-only v5 package API validates stream, segment and asset relationships when called. Desktop Stop uses the owner-produced index and closed summaries. A complete attempt can include accounted losses. Windows capture, installed trust, hardware profiles, performance and end-to-end operator behavior require verification against the [M1 acceptance criteria](SCREEN_FIX_M1_SPEC_ADDENDUM.md#6-revised-m1-acceptance-mapping).
