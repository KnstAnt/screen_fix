# M1 journal schema (package format v6)

Date: 2026-11-18 (Europe/Moscow)
Project: screen_fix / Intercall Recorder

## 1. Version and conventions

Package format v6 (`MANIFEST_VERSION = 6` in `core/src/package/manifest.rs`) versions `manifest.json` and every journal schema below. Rows carry no version of their own; a row's `version` field is only the foreground product version. Under D-1 A-v6only, `PackageReader::open` reads `version` first and opens v6 only; any other value is `UnsupportedVersion`. Packages recorded before v6 no longer open.

Marks:
- **PDF**: the kind or field is named literally by the Intercall Recorder technical spec v1 (Windows).
- **ext**: an extension. A record the PDF describes without naming the field is ext.

Shapes used below:
- Times ending in `_t_us`, `_us` or `recv_t_us` are integer microseconds on the session clock. A conversion from QPC or WGC time floors toward negative infinity, also before the clock origin. `_qpc` values are raw QPC ticks. `tick_ms` is the raw `GetTickCount` tick.
- **Evidence** is `{"status":"observed","detail":v}`, `{"status":"uncertain","detail":{"value":v,"reason":r}}` or `{"status":"unavailable","detail":r}`, with `r` one of `no_device`, `unsupported`, `not_observed`, `missing_calibration`, `ambiguous`, `blind`, `expired`.
- A point is `{"x","y"}` and a rectangle `{"left","top","right","bottom"}` in physical desktop pixels, right and bottom exclusive. A geometry id is `{"monitor","epoch"}`; a frame id is `{"monitor","epoch","sequence"}`; an event reference is `{"stream","seq"}`.
- "true or null" fields are always written and are `null` when the condition does not hold. Fields described as "absent" are omitted.
- Paths: a still `file` is relative to `raw/`, where `stills.jsonl` lives. Manifest `images[].file`, `cursors[].file` and a cursor asset's `image.png` are attempt-relative (`raw/...`).

## 2. Common row rules

Each journal is UTF-8 JSON Lines with LF endings, at most 1 MiB per row.

| Field | Mark | Rule |
| --- | --- | --- |
| `seq` | PDF | Written first. Starts at 1 and increases by one per row of the stream. |
| `recv_t_us` | PDF | Present on every row. Never decreases within a stream; ties keep `seq` order. |
| `source_t_us` | PDF | Present on every row; `null` when the provider has no session-clock time. |
| `kind` | PDF | Present on every row except in `frames.jsonl`. |

The order of the other fields is not significant.

### 2.1 Gap rows (`kind: "gap"`, ext)

Every stream except frames records ledger losses as typed gap rows. A pending loss is written before the stream's next row, at a boundary or at close. Privacy-sensitive losses (password, possibly sensitive or approval-unknown context, and key records lost before their visit is classified) never become rows; they are only counted in the manifest.

| Field | Meaning |
| --- | --- |
| `recv_t_us` | Admission receipt: the later of the triggering receipt and the stream's last row. |
| `source_t_us` | Always `null`. |
| `reason` | A `LossReason` (§2.2). |
| `producer` | A `LossProducer`, or `null` when unknown. |
| `count`, `count_known` | Lost items; `count` is `null` exactly when `count_known` is false. A known count is never 0. |
| `first_recv_t_us`, `last_recv_t_us` | Original receipt bounds of the lost items; `first ≤ last ≤ recv_t_us`. |
| `first_source_seq`, `last_source_seq` | Native source sequence range; absent together. |
| `first_qpc`, `last_qpc` | Native QPC range; absent together. |

Entries with the same `reason` and `producer` merge; the merged range is their hull and an unknown count stays unknown.

### 2.2 Loss and suppression vocabularies (`core/src/package/outcome.rs`)

`LossReason`:

| Value | Meaning |
| --- | --- |
| `queue_overflow` | The stream's bounded queue or byte budget was full (REL-2). |
| `late_commit` | A confirmed text commit arrived behind the published input prefix. |
| `staging_pressure` | A bounded staging or confirmation capacity could not hold the item. This includes the former `context_source_gap`. |
| `late_row` | The item's receipt precedes an already published or certified prefix. |
| `producer_stall` | The producer stopped advancing within its bounded wait. |
| `retention_unavailable` | Retained provisional evidence for an approved action no longer existed. |
| `native_queue_overflow` | The input helper's native queue or transport lost records. |
| `matcher_capacity` | The bounded input matcher could not hold the record. |
| `resync_pending` | The record arrived before the opening or post-gap held-state resync completed. |
| `foreground_inaccessible` | The receipt-time foreground was not accessible to the ordinary source. |
| `unknown_device` | The record named a device absent from the device inventory. |
| `hook_lost` | Required hook evidence was confirmed lost from the first missed receipt. |
| `malformed_packet` | A native packet or its encoded record was invalid. |
| `commit_capacity` | A bounded commit queue could not hold a finalized text commit. |
| `commit_too_long` | A finalized text commit exceeded the UTF-16 bound. |
| `commit_parts` | A finalized text commit had too many parts or an unreadable part array. |
| `commit_rejected` | A finalized text commit failed the sender, foreground or deadline check (counted without detail). |
| `clipboard_queue` | A bounded clipboard notice, grant or result queue was full. |
| `notification_overflow` | The bounded window notification ring or queue was full. |
| `uncertified_source` | Elevated records past the certified prefix of an interrupted visit (E1). |

Commit losses (`late_commit`, `commit_*`) are input-stream losses with producer `text_edit`, also in an elevated visit.

`LossProducer`: `text_edit`, `raw_input`, `device_notifications`, `keyboard_hook`, `pointer_hook`, `optional_hid`.

`SuppressionClass` (manifest counts only, never rows): `pause`, `not_approved`, `approval_unknown`, `recorder_control`, `control_uncertain`, `cluster_excess_hook`, `focus_uncertain`, `unconfirmed_synchronization`, `not_publishable`.

`UiaOutcome` (the `uia` value of a focus or element row without an element): `timeout`, `hung`, `unavailable`, `stale`, `outside_approval`. `UiaRead`: `focus`, `element`.

### 2.3 Health rows

A health condition is a `health` row in `context.jsonl` and `stills.jsonl`, and an `input_health` row in `input.jsonl` (INP-1, INP-12). The kind is PDF; the fields are ext. While a stream is paused, conditions are counted instead of written and reported in the context `resume` row (`suppressed_health`); a stream closed while paused writes one `paused_summary` row with `counts`.

| Field | Meaning |
| --- | --- |
| `health` | A `HealthKind` (below). |
| `component`, `failure` | The failing component and the failure object (`component`, `operation`, `stream`, `native_code`, optional `source`), without report detail. |
| `detected_t_us`, `first_affected_t_us` | Detection time and first affected time (`null` when unknown). |
| `ongoing` | Whether the condition persists. |
| `monitor`, `count` | Affected monitor and occurrence count, or `null`. |
| `counts` | `paused_summary` only: `{HealthKind: count}`. |

The input stream holds at most 32 pending health conditions. Beyond that, a condition merges into the latest pending one of the same kind (its `count` grows); a condition of a kind with nothing pending is not written and sets `input_capture.transport_overflow`.

`source_t_us` is `null`. `HealthKind`: `video_failure`, `hdr_white_level_unavailable`, `encoder_candidate_rejected`, `suspend_finalize_late`, `input_failure`, `attempt_failure`, `still_progress_stall`, `paused_summary`, `input_producer_stall`, `input_registration_lost`, `input_hook_lost`, `optional_collection_unavailable`, `identity_unresolved`, `auxiliary_activity_loss`, `still_write_failure`, `elevated_source_sleep`.

## 3. manifest.json

| Field | Mark | Content |
| --- | --- | --- |
| `version` | ext | `6`. |
| `attempt_id`, `state` | PDF | `state` is `complete` or `incomplete`. |
| `capture` | ext | `pending`, `recorded`, `stopped_before_first_frame`, `approval_suppressed`, `video_unavailable`, `input_unavailable` or `failed`. `approval_suppressed`: every slot of every monitor with slots was approval-suppressed. Approval is per approved process instance, so moving among windows of that process suppresses no slot. |
| `selected_displays` | ext | Selected monitor ids. |
| `clock` | PDF | `origin`, `utc_anchors[]` (`admission_epoch`, `sample`), `stop`, `last_admitted_boundary`. |
| `os` | PDF | Windows evidence. |
| `streams` | PDF | `{stream: StreamSummary}` (§3.1). The PDF's per-stream count is `rows`. |
| `health` | PDF | Health summary (§3.2). |
| `profile`, `monitor_capture`, `monitor_epochs`, `failed_step`, `monitor_video`, `input_capture`, `input_registration`, `source_evidence`, `png`, `images`, `cursors`, `disk_preflight` | ext | Inventories and producer counters. `input_capture.pen.pen_pressure` is PDF. `images` and `cursors` list `{file, bytes, width, height}` sorted by `file`. |

### 3.1 StreamSummary

| Field | Content |
| --- | --- |
| `rows`, `data_rows` | All rows; rows other than gap rows, health rows and a final `blind` row written after the input ingress closed. |
| `dropped`, `gap_rows` | Sum of known gap counts; number of gap rows. |
| `offered` | Items that reached a terminal outcome: data rows, known-count losses and suppressions. |
| `health_rows`, `pause_rows`, `blind_rows` | Rows of those kinds. |
| `suppressed_interval_rows`, `suppressed_interval_us` | `suppressed_interval` rows and their total duration. |
| `dropped_by_reason` | `{LossReason: {gap_items, unknown_gaps, sensitive_items, sensitive_unknown}}`; absent when empty. |
| `suppressed_by_class` | `{SuppressionClass: count}`; absent when empty. |
| `unavailable_by_reason` | `still_dropped` rows by `reason`; absent when empty. |
| `uia_unavailable` | `{UiaRead: {UiaOutcome: count}}` for focus and element rows without an element; absent when empty. |
| `paused_health` | Health conditions counted while paused, by `HealthKind`; absent when empty. |
| `buffered_flushes`, `os_flushes`, `os_flush_max_us`, `normal_entries_peak`, `prepared_bytes_peak` | Writer measurements. |
| `last_receipt`, `last_data_receipt` | Last row receipt and last data row receipt, or `null`. |

Unless a loss count is unknown, `data_rows` + the per-reason `gap_items` and `sensitive_items` + the suppression counts equal `offered`.

### 3.2 Health summary (`health`)

The summary is derived from the closed stream summaries and counts persisted rows only.

| Field | Mark | Content |
| --- | --- | --- |
| `pauses`, `gaps`, `blind_intervals`, `dropped_stills` | PDF | Sums of `pause_rows`, `gap_rows`, `blind_rows` and the stills `unavailable_by_reason` counts. |
| `recovered_prefix` | PDF | Always false in M1 (REL-3 is deferred). |
| `suppressed_intervals`, `suppressed_interval_us` | ext | Sums over streams; absent when 0. |
| `streams` | ext | `{stream: {dropped_by_reason, suppressed_by_class}}` for streams with losses or suppressions. |
| `activity_losses` | ext | `[{component, activity, monitor, operation, native_code, first_t_us}]` in report order; `activity` is `required` or `auxiliary`. |
| `omitted_health` | ext | Capture and video conditions beyond the runtime row bound; absent when 0. |
| `undelivered_frames` | ext | `[{monitor, first_slot, after_t_us}]` for a video worker blocked in a native call (§6.2 row 21, an attempt failure). |

## 4. input.jsonl

### 4.1 Fields shared by published input rows

Key, pointer, scroll, pen and HID rows carry:

| Field | Mark | Content |
| --- | --- | --- |
| `origin` | PDF | `hardware`, `touchpad`, `pen_promoted`, `touch_promoted`, `injected` or `unknown`. |
| `device` | PDF | Evidence of the device id. |
| `tick_ms` | PDF | Hook or Raw Input message tick, or `null`. |
| `injector_hints` | PDF | Injected rows only. |
| `window` | ext | Receipt-time foreground window id. |
| `correlation` | ext | `matched`, `missing_scan`, `ambiguous`, `unmatched` or `discontinuous`. |
| `cluster_raw`, `cluster_hooks` | ext | Ambiguous cluster rows only. |
| `cursor_id` | ext | Promoted pen or touch rows only. |
| `admission_epoch`, `source_uncertainty_us`, `tick_uncertainty_us`, `late` | ext | `late` is `null` when unclassifiable or based on an injected hook. |
| `pen_promotion_link` | ext | Approved pen promotion only: `{pen_report, pointer_report, receipt_gap_us}`. |
| `touchpad_contact_witness` | ext | Touchpad-attributed rows only: `{report_reference, receipt_gap_us}`. |
| `source_device` | ext | Touchpad-attributed rows only: the reporting device (Evidence), which the held state and its synthetic edges use; `device` names the touchpad. |

### 4.2 Keys

`key_down`, `key_up`:

| Field | Mark | Content |
| --- | --- | --- |
| `kind`, `vk`, `key` | PDF | Virtual key and logical key name. |
| `scan` | PDF | Make code with the E0/E1 prefix folded (0xE0xx, 0xE1xx), or `null`. `null` for `VK_PACKET`. |
| `mods` | PDF | Side names held after the transition and known: `shift_l`, `shift_r`, `ctrl_l`, `ctrl_r`, `alt_l`, `alt_r`, `win_l`, `win_r`. |
| `repeat` | PDF | `key_down`: true for a down while already published down, false for a known press, `null` when unknown. Always `null` on `key_up`. |
| `scan_prefix` | ext | `none`, `e0`, `e1`, or `null`. |
| `raw_input`, `keyboard_hook` | ext | The two witnesses, `null` when missing. |
| `mods_known` | ext | Sides whose state is known; a side absent here is unknown. |
| `held_across_boundary`, `press_unavailable` | ext | true or null. |
| `typing` | ext | `non_text`, `inferred`, `unknown`, `ime_composing`, `dead_key` or `injected_unicode`. |
| `ime`, `layout`, `focus_generation` | ext | `ime` only on editable-target downs. |
| `ndof_button_candidates` | ext | Injected key rows only: `{status: "candidate_only", rivals_present, candidates[{device, report_reference, gap_us}], truncated}`. |

### 4.3 Pointer and scroll

`pointer_down`, `pointer_up`, `pointer_move` and `scroll` carry these pointer fields:

| Field | Mark | Content |
| --- | --- | --- |
| `pos`, `pos_recv` | PDF | Evidence of a point: the event's hook point, and `GetCursorPos` at receipt. |
| `monitor` | PDF | Evidence of the monitor id. |
| `held` | PDF | Buttons (1–5) held after the row. |
| `mods` | PDF | Side names held after the row. |
| `delta` | PDF | Evidence of the summed raw `[dx, dy]`. |
| `coalesced` | PDF | Raw packets merged into the row, minus one. |
| `warp` | PDF | Evidence of a position jump. |
| `geometry` | ext | Evidence of the geometry id. |
| `pos_receipt_t_us`, `pos_recv_receipt_t_us` | ext | Receipts of the records that carry the two points. |
| `raw_absolute`, `raw_absolute_xy`, `raw_move_flags`, `raw_packets` | ext | Absolute-mode evidence and packet count. |

Button rows add `button` (PDF, 1–5) and `button_physical` (PDF, Evidence), plus `pen_promotion`, `press_unavailable` (true or null), `physical_press` (false on approved promotions, else null) and `element` (ext). `element` is a link to an element read: `{request_id, age_us, basis: pre_action|post_action|pointer_rest, reason}`, with `reason` (`pointer_query_busy`) absent when a read is linked, or `null` when no read applies. `pointer_move` has no button fields. `scroll` adds `axis`, `wheel`, `raw_wheel` and `hook_wheel` (ext; the last two are Evidence).

### 4.4 Pen and HID reports

`pen` (PDF) carries `kind`, `report_id`, `sub_index`, `xy` (`[x|null, y|null]`, raw logical), `tip`, `in_range`, `pressure` (0–1), `pressure_raw`, `tilt` (`[deg, deg]`), `tilt_raw` (signed scaled reading), `barrel`, `eraser`, `invert`, `scan_time` and `device` (all PDF), and `secondary_barrel`, `twist`, `twist_raw` (ext). A lifted key is absent when the report does not declare its usage, or declares it in several link collections; a declared but unreadable or null-state value is `null`.

HID rows (ext) of kinds `pen`, `touchpad_report`, `ndof_unavailable` and `hid_unavailable` also carry `usage_page`, `usage`, `report_reference`, `source_clock` (`hid_scan_time_sliding_minimum` or `null`) and `fields[]`, each `{link_collection, usage, value | value_state: "unavailable", declared_range, normalized (pressure), angle_reading, degrees}`. `touchpad_report` adds `contact_frame`. Pen rows add `contact_transition`, `position_at_receipt` and `position_at_receipt_geometry`. A report that could not be decoded has `report` (the result) instead of `fields`. `ndof` rows carry `report_id`, `sub_index`, `report_reference`, `device_kind: "spacemouse_profile"` and `data`. A pen row's `source_t_us` is the fitted HID Scan Time when available.

### 4.5 Synthetic edges and synchronization

Synthetic edges close or reopen held state at a boundary. They carry `synthetic: true`, `boundary` (`input_gap`, `device_removed`, `admission_closed`, `pause`, `stop`, `focus_changed`, `recorder_control`, `hook_unavailable`, `hid_report_unavailable`, `synchronization`), `device` (Evidence) and `admission_epoch`, all ext, and no `origin`, `tick_ms` or pointer fields:
- keys: `key_down`/`key_up` with `vk`, `scan`, `scan_prefix`, `raw_input: null`, `keyboard_hook: null`, `mods: []`, `mods_known: []`, and no `key`;
- buttons: `pointer_down`/`pointer_up` with `button` and `button_physical`;
- promoted pen buttons: `pointer_down`/`pointer_up` with `button`, `pen_promotion: true` and `cursor_id`;
- pen tips: `pen_down`/`pen_up` (ext kinds) with `link`.

`held_state_sync` (ext) precedes the edges of a verified synchronization: `synthetic: true`, `focus_generation`, `admission_epoch`.

### 4.6 Text, privacy, blind and health rows

| Kind | Mark | Fields |
| --- | --- | --- |
| `text_commit` | PDF | `text`, `status` (`inferred`, `confirmed`, `injected`, `invalid`, `unknown`; PDF); `provenance` (`layout_inferred`, `injected_unicode`, `text_edit_composition_finalized`), `focus_generation`, `admission_epoch`, `text_edit_receipt_t_us` (ext). |
| `redacted` | ext | `field_start_t_us`, `field_end_t_us`, `key_count`, `admission_epoch`: one row per password or possibly sensitive field visit. |
| `suppressed_interval` | ext | `reason` (`ineligible_token`, `elevated_source_unavailable`, `non_approved_app`, `unknown_foreground`), `start_t_us`, `end_t_us`, `input_suppressed` (count), `admission_epoch` (C5, D-3 A). |
| `blind` | PDF | `reason`, `start_us`, `end_us` (PDF); `producers` (ext, present only when the interval covers some producers). `reason` is an input blind reason (`helper_exited`, `registration_lost`, `hook_lost`, `helper_communication_failed`, `helper_cleanup_failed`, `producer_stall`, `resync_unavailable`, `elevated_window`, `foreground_inaccessible`, `device_removed`) or, for a pause interval, `secure_desktop`, `elevation_transition`, `user`, `sleep` or `low_disk`. |
| `input_health` | PDF kind | A health condition (§2.3), or per-second counts (ext): `session_second`, `raw_packets_coalesced`, `motion_rows_thinned`, `suppressed` (`{class: count}`; an AltGr companion key is counted only in the stream summary), `raw_motion[{device, packets, delta}]`, `admission_epoch`. |
| `gap` | ext | §2.1. |

## 5. context.jsonl

### 5.1 Envelope

Rows written through the context owner carry `tick_ms` (PDF) and `source_uncertainty_us`, `tick_uncertainty_us`, `late` and `admission_epoch` (ext). Rows triggered by a matching WinEvent with a tick add `win_event_recv_t_us` (ext); their `tick_ms`, `tick_uncertainty_us` and `late` come from that notification, and `recv_t_us` is the context poll. Gap and health rows carry only the §2 fields. Rows built from an elevated collector observation add `context_receipt_t_us` (ext); window and hung reads add `observed_qpc` (ext). The v5 envelope `version` is removed.

### 5.2 Windows

`foreground` (PDF), approved: `approved: true`, `pid`, `exe`, `product`, `version` (dotted fixed file version), `version_string`, `rect`, `monitor`, `title`, `class` (PDF); `window`, `version_observation: "approved_file_at_startup"`, `geometry`, `dpi`, `dpi_awareness`, `metadata_observed_at_read: true`, `ghost_replacement` (on a verified Ghost replacement) (ext). Variants:
- not approved: `{kind, approved: false}` only;
- approved but unreadable metadata: `{kind, approved: true, window, pid, metadata: "unavailable"}`;
- elevated read without evidence: `{kind, approved, window, native_status}` with `native_status` `timed_out`, `unavailable`, `stale` or a read outcome; a window notice already older than the read budget when handled is `{kind, approved: false, native_status: "timed_out"}`.

In `exe`, `product` and `version_string`, the values of `USERPROFILE`, `COMPUTERNAME` and `USERNAME` are replaced by `<home>`, `<host>` and `<user>`. A value matches ignoring ASCII case, and only as a whole token or path segment (no adjacent letter or digit).

`window_title`, `window_location`, `window_shown` and `window_hidden` (ext kinds) have the same shapes. Rows caused by a notification add `source_event_tick_ms`; a rate-limited location re-read, including one read through the elevated collector, adds `trailing: true`.

### 5.3 Focus and element

`focus` (PDF): `uia`, `caret`, `ime` (PDF); `window`, `target`, `cursor`, `layout`, `dpi`, `dpi_awareness`, `approved`, `request_id`, `generation`, `requested_t_us`, `started_t_us`, `completed_t_us` (ext). `caret` is non-null only for an editable target.

`element` (PDF): `trigger` (`pointer_down` or `pointer_rest`), `pos` (a point), `uia` (PDF); `window`, `geometry`, `action_t_us`, `post_action`, `action_to_completion_us`, `dpi`, `dpi_awareness`, `approved`, `request_id`, `generation`, `requested_t_us`, `started_t_us`, `completed_t_us` (ext). Outside approval: `{kind: "element", approved: false}`.

`uia` is an element object, a `UiaOutcome` string, or `null` for a ready read without an element. The object holds `control` (control type name or `null`), `framework`, `editable`, `password`, `name`, `rect` (PDF) and `control_type_id`, `automation_id`, `class`, `ancestor_names` (ext).

### 5.4 Other context rows

Kinds are ext unless marked; fields are ext unless marked.

| Kind | Fields |
| --- | --- |
| `cursor` (PDF) | `shape`, `visible`, `suppressed` (PDF); `approved`, `cursor_observed_t_us`, `custom_asset` (`not_needed`, `pending`, `unavailable`), `custom_asset_pending`, `custom_asset_reason`. Unapproved: `{kind, approved: false}`. |
| `cursor_asset` | `cursor_observed_t_us`, `available`, `image` (`{png, xor_mask, hotspot}` when available, else `null`). |
| `keyboard_layout` | `generation`, `layout`. |
| `ime_state` | `generation`, `ime` (`not_ime`, `closed`, `pass_through`, `composing`, `unknown`); elevated rows add `started_t_us`, `completed_t_us`. |
| `caret` | `generation`, `change` (`show`, `hide`, `location`), `visible`, `bounds`. |
| `app_busy` | `approved`, `hint`; hung-process rows add `scope: "process_hung"`, `process` and `hint: {window, level, reasons}`. |
| `window_hint` | `hint` (`minimize_start`, `minimize_end`, `switch_start`, `switch_end`, `ime_show`, `ime_hide`, `ime_change`), `approved`, `window` (`null` when not approved). |
| `app_hung` (PDF) | `window`, `process`, `thread_id`, `active`, `reason` (`detected`, `responsive`, `observation_ended`), `native_hung`, `ghost_replacement`. |
| `app_hung_ghost` | `active`, `association` (`unknown`, `confirmed` or `null`); confirmed rows add `window`, `process`. |
| `app_hung_coverage` | `complete`; elevated rows add `source: "elevated"`. |
| `display` | `available`; when available, `monitors[{display_key, selected_monitor, geometry, bounds, dpi_x, dpi_y, refresh, rotation, hdr, primary, unavailable}]`. |
| `monitor_missing`, `monitor_returned`, `display_reconfiguring` | `monitor`, `geometry`. |
| `clipboard_change` | `approved`, `reason` (`too_large` or `null`), `text`, `sequence`, `owner_verified`, `notification_receipt_t_us`, `completed_t_us`. |
| `pause` (PDF) | `reason` (PDF; the first of `reasons`, `null` when empty), `reasons` (`user`, `lock_screen`, `sleep`, `secure_desktop`, `low_disk`, `elevation_transition`), `cutoff_t_us`, `uncertified_input_tail`, `paused_slots`, `paused_slots_all_monitors`, optional `required_hook_first_uncovered_qpc`. |
| `resume` (PDF) | `reason` (PDF), `reasons`, `paused_start_t_us`, `paused_slots`, `suppressed_context_notifications`, `suppressed_health` (`{HealthKind: count}` for context and input). |
| `discontinuity` | `start_t_us`, `end_t_us`, `gap_us`, `previous_admission_epoch`, `reasons`. |
| `recorder_control` | `index`, `admitted_100ns`, `command`, `outcome`, `request_id`, `save_context`, `control_receipt_t_us`. The Stop row is `command: "stop_boundary"` with `cutoff_t_us`, `stop_t_us`, `paused_slots` and optional `required_hook_first_uncovered_qpc`. |
| `recorder_control_gap` | `index`, `admitted_100ns`, `dropped`, `first_control_receipt_t_us`, `last_control_receipt_t_us`. |
| `checkpoint_context` | `request_id`, `control_receipt_t_us`, `save_context`, `status` (`observed`, `unavailable`), `reason` (`queue_full`, `timed_out`, `admission_closed` or `null`). |
| `input_source_loss` | `source: "elevated"`, `visit`, `first_uncovered_qpc`, `cause`; then `native_gap`, or for `cause: "recorder_staging"` `native_gaps` and `malformed`. |
| `input_source_coverage` | `source: "elevated"`, `coverage`, `admission_epoch`, `registration`. |
| `gap`, `health` | §2.1, §2.3. Recorder staging loss of elevated context is a gap row with `reason: "staging_pressure"`. |

Blind rows are in `input.jsonl`.

## 6. stills.jsonl

One row per trigger. `recv_t_us` is the trigger receipt. A pre-motion or pre-action row joins its `before` and `seen` frames (IMG-2).

| Field | Mark | Content |
| --- | --- | --- |
| `kind` | PDF | `still` or `still_dropped`. |
| `source_t_us` | PDF | Present time (WGC `SystemRelativeTime`) of the chosen frame, or `null`. |
| `monitor`, `trigger` | PDF | `trigger`: `start`, `pre_motion`, `pointer_rest`, `pre_action`, `next`, `settled`, `observe`, `checkpoint`, `final`. |
| `frame_obs_id` | PDF | Sequence of the chosen frame, or `null`. |
| `event_ref` | PDF | Causal input row `{stream: "input", seq}`, or `null`. |
| `file` | PDF | `images/<digest>.png` relative to `raw/`, or `null`. |
| `width`, `height` | PDF | Image size, or `null`. |
| `seen_obs_id` | PDF | `pre_motion` and `pre_action` rows; may be `null`. |
| `seen_file` | PDF | Only when the seen image differs from `file`. |
| `role` | ext | `before`, `image` or `final_save_settled`. |
| `trigger_t_us`, `reason`, `geometry`, `admission_epoch`, `scroll_device`, `settled_confirmed_t_us`, `adjustment`, `element` | ext | `reason` explains a `still_dropped` row. |
| `frame` | ext | `{id, source_t_us, recv_t_us, approval_generation, width, height, color, source_uncertainty_us}`. `approval_generation` identifies the frame approval in force. An ordinary approval is per approved process instance: the generation changes when the approved process or the admission epoch changes, when an interruption occurs (lost notice, desktop switch, a foreground notice for another process, an unreadable window or a Ghost window), on a stale observation, or after the approval was cleared. Another window of the same process keeps it. An elevated approval has its own generation per visit (ALG §13.14). |
| `outcome` | ext | `{kind: saved, image, deduplicated}`, `{kind: dropped, reason}`, `{kind: failed, operation, native_code}` or `{kind: unavailable, reason}`. |
| `save` | ext | `{id, kind, routing_basis}` for save-linked stills. |
| `seen_frame`, `seen_outcome` | ext | Pre rows only. |
| `fallback_reason`, `geometry_evidence` | ext | Approved fallback rows only: `geometry_evidence` on every approved fallback, `fallback_reason` when the slot expired (`reason` is then `still_ordered_deadline` or `still_owner_closed`). |

The v5 `schema_version` field is removed. The stream also carries gap and health rows (§2).

## 7. frames.jsonl

One row per decided, non-paused slot per selected monitor, without `kind`. The stream has no gap rows; rows are missing only after an attempt failure (`undelivered_frames`).

| Field | Mark | Content |
| --- | --- | --- |
| `monitor` | PDF | Monitor id. |
| `obs_id` | PDF | Sequence of the selected or repeated frame; absent without a frame. |
| `source_t_us` | PDF | Present time (WGC `SystemRelativeTime`) of that frame; `null` without a frame. |
| `recv_t_us` | PDF | Capture receipt or ordering receipt (below). |
| `pts_us` | PDF | Slot start on the session clock. |
| `encoded` | PDF | Whether the slot has an encoded sample. |
| `segment`, `frame_in_segment` | PDF | Encoded rows only. |
| `repeated` | PDF | Absent without a frame. |
| `change` | PDF | Change score; only on rows that newly select a frame and have a score. |
| `geometry_version` | PDF | Geometry epoch of the frame; absent without a frame. |
| `slot`, `slot_decided_t_us`, `published_t_us`, `admission_epoch` | ext | Always present. |
| `reason` | ext | Missing-slot reason, on rows without an encoded sample. |
| `pts_100ns`, `duration_100ns` | ext | Decoded-local sample time; encoded rows only. |
| `frame_id`, `capture_recv_t_us`, `geometry` | ext | Frame identity, its capture receipt (max of the dequeue and the present time) and monitor geometry; absent without a frame. |
| `late` | ext | Repeat rows with a frame: whether a changed frame of this slot arrived after the decision. |

`reason` values (`MissingVideoSlot`): `before_first_frame`, `approval_suppressed`, `capture_unavailable`, `monitor_missing`, `conversion_pressure`, `queue_pressure`, `encoder_unavailable`, `segment_unpublished`, `encoder_skipped`, `timeline_pending`, `segment_unverified`. An `approval_suppressed` slot has no approval covering it; a change of approval generation also ends the open segment. Because the generation changes only with the approved process, the epoch or an interruption, switching among windows of the approved process neither suppresses slots nor cuts the video.

**`recv_t_us` of frames rows (plan §6.5, amended).** Let e_k be the end of slot k, limited by the epoch cutoff.
- `recv_t_us` is the true capture receipt only on a row that newly selects a frame (`repeated: false`). It then equals `capture_recv_t_us`.
- On a repeated row or a row without a frame, `recv_t_us` is an **ordering receipt**: max(`recv_t_us` of the monitor's previous row, e_k), or e_k for the monitor's first row. It only keeps the stream's receipt order and is not an observation time. An `approval_suppressed` row withdrawn after it selected a frame keeps that withheld frame's receipt as its ordering receipt.
- Timing analysis uses `slot_decided_t_us` (slot decision), `source_t_us` (frame present time; it can be later than the dequeue, ALG §13.14) and `capture_recv_t_us` (frame receipt), never `recv_t_us` of a repeated or frameless row.
- By design, a repeated or frameless row's `recv_t_us` can be less than its `slot_decided_t_us`, because the ordering receipt is the slot end bounded below by the previous row. Consumers must not treat that difference as an anomaly.

A repeated row keeps the `obs_id`, `source_t_us`, `frame_id` and `capture_recv_t_us` of the repeated frame. Monitors are merged by (`recv_t_us`, `monitor`, `slot`).

## 8. Validation

`PackageReader::open` checks the manifest: `version` is 6; `selected_displays` is non-empty, unique and within the monitor bound; `state: complete` exactly when `capture: recorded`; `failed_step` exactly when `capture: failed`; `health` equals its recomputation from `streams`, and a complete attempt has no `undelivered_frames` and no required activity loss; `source_evidence` intervals are valid; and a complete attempt with `profile` passes the recorded-inventory check.

`PackageReader::validate_gates` reads each listed journal once, context first, then input, frames and stills. A complete attempt with `profile` is checked strictly: every stream summary must exist. Listed segment files must exist with their `published_bytes`.

| Stream | Checks |
| --- | --- |
| All (gate 1) | Each line ends with LF, parses, has `seq` = previous + 1 and a non-decreasing `recv_t_us`. Gap rows deserialize and are valid (§2.1). Health rows have a receipt and a known `HealthKind`. At the end: `rows`, `last_receipt` and `dropped` match the summary, `gap_rows` matches when `profile` is present, and with a `health` block the per-reason gap totals, `unavailable_by_reason`, `uia_unavailable`, `health_rows`, `pause_rows`, `blind_rows`, `suppressed_interval_rows`/`suppressed_interval_us` and the `offered` identity reconcile. |
| context | Gate 4: between a `pause` row and its `resume` row only `pause`, `resume`, `discontinuity`, `gap`, `health`, `recorder_control`, `recorder_control_gap`, `input_source_loss` and unapproved `foreground` rows. A pause interval runs from its `cutoff_t_us` to the resume receipt, or to the end when not resumed. `cursor_asset` rows: an available asset references an inventoried cursor PNG and an XOR mask of the same size or `null`; an unavailable one has `image: null`. |
| input | Gate 2 action rows: non-synthetic `key_down`, `key_up`, `pointer_down`, `pointer_up`, `pointer_move`, `scroll`, `pen`. Gate 3: presses are keyed by `vk` or `button` and the observed `source_device`, else `device`; a down opens a press unless `repeat` is true or `null` (synthetic downs always open); a down for an open press, a non-synthetic release without a press or `press_unavailable`, a press open across a pause cutoff, and a press never released are findings, excused when the span overlaps a pause (a press open across a pause cutoff is not excused by that pause), `blind` (`start_us`–`end_us`), gap (`first_recv_t_us`–`last_recv_t_us`), `discontinuity` or `suppressed_interval` interval. Pen tip and contact transitions are not part of gate 3 (spec addendum A7.10). Gate 4: inside a pause interval only `gap`, `blind`, `input_health` and `suppressed_interval` rows. |
| frames | Rows without `kind` deserialize; `monitor` is selected and matches `frame_id` and `geometry`, whose epoch equals `geometry_version`. Strict: per-monitor `slot` is contiguous from 0, except that it jumps from `first_paused_slot` to `end_slot_exclusive` across a paused range that a context `resume` row names in `paused_slots`, and a row with `kind` fails. Encoded rows: a listed segment, `pts_100ns` and a positive `duration_100ns`, a `frame_id`, no `reason`, `frame_in_segment` consecutive from 0, a first PTS within 1 ms of 0, increasing PTS, the segment's admission epoch, first frame and first geometry, and at most `frame_count` rows. Rows without a sample: a `reason` and no segment, index, PTS or duration. Strict end: each segment's frame count and end PTS, and each monitor's `slots` (its count of frames rows), match. Gate 4: no row whose `pts_us` lies inside a pause interval. |
| stills | A non-null `event_ref` names an input action row within the input stream (gate 2). A `still` row has a `geometry` and `frame.id` on the same selected monitor, and its `raw/` + `file` is an inventoried PNG whose header, size and dimensions match `images` and the row's `width` and `height`. A `seen_file` differs from `file` and is a valid inventoried PNG. Gate 4: no `still` or `still_dropped` row whose `trigger_t_us` lies inside a pause interval. |

The decode comparison of gate 2 is not part of `validate_gates`: the per-monitor segment verifier decodes each finalized segment before publishing it, and `package::verify_attempt` (the `verify_attempt` example) decodes it again on a closed package.
