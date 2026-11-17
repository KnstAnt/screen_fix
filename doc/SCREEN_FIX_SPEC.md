# screen_fix specification and roadmap

screen_fix / Intercall Recorder collects a local record of a person's work on a Windows desktop for downstream training and evaluation. The first supported platform is Windows 10 version 2004 (build 19041) or later, including Windows 11, on x64. The [Windows technical specification](<Intercall Recorder technical spec v1 Windows.pdf>) defines the product. The [M1 specification addendum](SCREEN_FIX_M1_SPEC_ADDENDUM.md) supplies the accepted amendments. The [architecture](SCREEN_FIX_ARCHITECTURE.md) describes the current capture core.

## 1. Recording scope

M1 covers the capture core. A contributor approves an executable and chooses the local destination, displays and capture pace. Start and Stop are explicit. The Recorder shows preparation, recording, pause and final outcomes. Its non-activating status indicator can appear in captured pixels. Pause before entering private information. Full own-window exclusion, blackout and review belong to M3. M1 writes locally and leaves sharing to the contributor. Automated transfer belongs to a later milestone.

Admission applies to the actual approved foreground and its evidence. The Recorder launches asInvoker and uses its existing token. A normal launch from the built-in Windows Administrator can inherit high integrity. An approved foreground within that token’s access scope uses the standard source. For an approved elevated foreground that needs additional access, an eligible Recorder can use the separate demand-started collector on the same unlocked interactive Windows desktop, account, session and linked UAC sign-in. The approved exception to INP-13 and its Start preflight and gate 3 treatment are defined in the [M1 amendments](SCREEN_FIX_M1_SPEC_ADDENDUM.md) and the [elevated capture specification addendum](SCREEN_FIX_ELEVATED_COMPONENT_SPEC_ADDENDUM.md). Protected capture needs the official matching signed Recorder, standard helper and collector installed machine-wide in the protected x64 Program Files location. Ordinary portable and development use remains available. Cross-user elevation, other sessions or desktops, SYSTEM and protected targets remain outside the exception. Unapproved or uncertain foregrounds and periods without coverage have explicit suppression, blind, gap or unavailable outcomes as applicable.

## 2. Evidence and files

| Area | M1 contract |
| --- | --- |
| Video | Selected monitors provide WGC frames and independently paced H.264/MP4 segments. Startup checks the profile and chooses a supported encoding path. The index distinguishes real, repeated and missing slots, original source time and segment-local presentation time. Admission, pause and source changes bound publication. |
| Input | Record available keyboard, mouse and supported pen/device evidence, with provenance, device and observed position as applicable. Match only within the active source visit. Synthetic boundary records and unknown state distinguish resynchronization from a physical transition. |
| Context | Record approved window, focus, geometry and eligible text/UI Automation evidence. Clipboard text is opt-in, with sequence, owner and focus checks on a separate worker. An observed clipboard change does not establish paste. Password and uncertain sensitive fields follow the conservative publication rules. |
| Stills | Select native cursor-free observations through the [M1 algorithms](SCREEN_FIX_M1_ALGORITHMS_ADDENDUM.md). Preserve timing, geometry, available event references, selection provenance and explicit unavailable outcomes. Deduplicate exact stored images. A checkpoint hotkey creates a request. A Save still requires confirmed success evidence, provided by a trusted fixture route in M1. |
| Package | Publish a local attempt with a manifest, independent journals, video segments and available still and cursor assets. The current v5 reader exposes known source coverage and optional selected-source evidence. v5 consumers use native still references. |

Use a common monotonic QPC timeline across epochs, pauses and handoffs. Preserve original observation and source timing, processing receipt and media presentation as separate values where present. Each journal orders receipts independently and uses sequence to break ties. Equal timestamps across streams establish no causal order. Keep observed facts separate from derived interpretation. A later cursor observation is separate from input coordinates, and observed focus does not establish an input recipient. A first post-action frame and an observed settled result have different meanings.

## 3. Completion and validation

Stop before admission cancels preparation. Once admitted, Stop closes admission, settles the evidenced prefix within the existing bounds, finalizes the required media and journals, and publishes the manifest. Stop from pause preserves the earlier admission cutoff. Complete denotes verified required closure and usable video for each selected monitor. Accounted losses can still appear in a complete attempt. An admitted protected-source failure retains the first substantial cause and may expose a failed package only after manifest publication succeeds.

Detected failures produce an operator-visible outcome and a bounded local diagnostic report when possible. Reports use the chosen output directory and a temporary path is used only before an output directory is selected. A failed write to a chosen location is reported as unavailable. Diagnostics avoid captured secrets. Automatic repair, source-worker replacement, restart, resumption and automatic diagnostic transmission are outside M1.

Windows execution, deployment, signing, hardware compatibility, timing, resource budgets, image readability and target-app impact require their applicable verification. Use the PDF and accepted M1 amendments for scenario and acceptance criteria.

## 4. Later milestones

| Milestone | Scope |
| --- | --- |
| M2 | Task, application-pack and file workflows, including production Blender and FreeCAD integration |
| M3 | Full privacy controls, own-window image exclusion, review and editing |
| M4 | Normalization, export views, consent, upload and server processing |
| M5–M6 | Adobe and 3ds Max application integrations as defined in the PDF |
