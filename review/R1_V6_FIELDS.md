# R1 package 2: v6 field list (developer report, input for D1 schema)

PDF = named by the PDF; ext = extension. This list is subject to the critic's review of package 2.

## Manifest
- `version` = 6.
- `StreamSummary.uia_unavailable` (ext): {`focus`|`element` → {outcome → count}}.
- `unavailable_by_reason` (ext): still_dropped reasons only.

## Common fields
Every row starts with `seq`, `recv_t_us` and `source_t_us` (PDF). `source_t_us` is null when not set.

## input.jsonl
- **key_down / key_up (ordinary)**
  - PDF: `kind`, `scan` (int, E0/E1 folded, or null), `vk`, `key`, `mods` (list of side names), `repeat` (down only; null when unknown), `origin`, `device`, `tick_ms`, `injector_hints` (injected rows only).
  - ext: `scan_prefix` (none/e0/e1/null), `raw_input`, `keyboard_hook`, `mods_known`, `held_across_boundary`, `press_unavailable`, `typing`, `ime`, `layout`, `focus_generation`, `window`, `correlation`, `cluster_raw`/`cluster_hooks` (cluster only), `cursor_id` (promoted only), `ndof_button_candidates`, `admission_epoch`, `source_uncertainty_us`, `tick_uncertainty_us`, `late`.
- **pointer_down / pointer_up**
  - PDF: `kind`, `button` (1–5), `button_physical` (Evidence), `pos`, `pos_recv`, `monitor`, `held` (list of 1–5, after the event), `mods`, `delta` ([dx,dy] Evidence), `coalesced`, `warp`, `origin`, `device`, `tick_ms`, `injector_hints`.
  - ext: `pen_promotion`, `press_unavailable`, `physical_press`, `element` (link: `request_id`, `age_us`, `basis`, `reason`), `geometry`, `pos_receipt_t_us`, `pos_recv_receipt_t_us`, `raw_absolute`, `raw_absolute_xy`, `raw_move_flags`, `raw_packets`, `pen_promotion_link`, `touchpad_contact_witness`, plus the common ext fields of key rows from `window` onward.
- **pointer_move**: the pointer fields above, without the button fields.
- **scroll**: the pointer fields, plus `axis`, `wheel`, `raw_wheel`, `hook_wheel` (ext).
- **pen** (critic fix, INP-11):
  - PDF: `kind`, `report_id`, `sub_index`, `xy` ([x|null, y|null], raw logical), `tip`, `in_range`, `pressure` (0–1), `pressure_raw`, `tilt` ([deg, deg]), `tilt_raw` (signed scaled reading), `barrel`, `eraser`, `invert`, `scan_time`, `device`.
  - ext: `secondary_barrel`, `twist` (deg), `twist_raw`, plus the HID fields below.
  - A key whose usage the report does not declare, or that is declared in several link collections, is absent. A declared but unreadable or null-state value is null.
- **pen / touchpad_report / ndof / ndof_unavailable / hid_unavailable** (ext; HID rows unchanged)
  - Fields: `report_id`, `sub_index`, `usage_page`, `usage`, `fields`, `report_reference`, `source_clock`, `contact_frame`, `report`, `data`, `device_kind`.
  - Pen only: `contact_transition`, `position_at_receipt`, `position_at_receipt_geometry`.
- **Synthetic edge rows**
  - Common: `synthetic:true`, `boundary`, `device`, `admission_epoch`.
  - Key: `vk`, `scan`, `scan_prefix`, `raw_input:null`, `keyboard_hook:null`, `mods:[]`, `mods_known:[]`.
  - Button: `button`, `button_physical`.
  - Promoted pen: `button`, `pen_promotion:true`, `cursor_id`.
  - Pen tip: `link`.
- **held_state_sync**: `synthetic`, `focus_generation`, `admission_epoch`.
- **blind**: `kind`, `reason`, `start_us`, `end_us` (PDF); `producers` (ext, when partial).
- **input_health** (the kind is PDF; the fields are ext)
  - Per-second: `session_second`, `raw_packets_coalesced`, `motion_rows_thinned`, `suppressed`, `raw_motion`[{`device`,`packets`,`delta`}], `admission_epoch`.
  - Condition: `health`, `component`, `failure`, `detected_t_us`, `first_affected_t_us`, `ongoing`, `monitor`, `count`.
  - `paused_summary`: `health`, `counts`.
- **gap** (ext): `reason`, `producer`, `count`, `count_known`, `first_recv_t_us`, `last_recv_t_us`, and when present `first_source_seq`/`last_source_seq`/`first_qpc`/`last_qpc`.
- **text_commit**: `text`, `status` (PDF); `provenance`, `focus_generation`, `admission_epoch`, `text_edit_receipt_t_us` (ext).
- **redacted** (ext): `field_start_t_us`, `field_end_t_us`, `key_count`, `admission_epoch`.
- **suppressed_interval** (ext): `reason`, `start_t_us`, `end_t_us`, `input_suppressed`, `admission_epoch`.

## context.jsonl
- **Envelope:** `recv_t_us`, `source_t_us`, `tick_ms` (PDF); `source_uncertainty_us`, `tick_uncertainty_us`, `late`, `admission_epoch` (ext). The envelope `version` is removed.
- **foreground**
  - PDF: `kind`, `approved`, `pid`, `exe`, `product`, `version` (dotted fixed file version), `version_string` (FileVersion string), `rect` {left,top,right,bottom}, `monitor`, `title`, `class`.
  - ext: `window`, `version_observation`, `geometry`, `dpi`, `dpi_awareness`, `metadata_observed_at_read`, `ghost_replacement`, `win_event_recv_t_us`.
  - Unapproved: `{kind, approved:false}`.
  - Metadata unavailable: `{…, window, pid, metadata:"unavailable"}`.
  - Elevated rows add `native_status`, `observed_qpc`, `context_receipt_t_us`.
  - `window_title`, `window_location`, `window_shown`, `window_hidden`: the same field set, plus `source_event_tick_ms` and `trailing`.
- **focus**
  - PDF: `uia`, `caret`, `ime`.
  - ext: `window`, `target`, `cursor`, `layout`, `dpi`, `dpi_awareness`, `approved`, `request_id`, `generation`, `requested_t_us`, `started_t_us`, `completed_t_us`, `context_receipt_t_us` (elevated).
- **uia object**
  - PDF: `control` (name or null), `framework`, `editable`, `password`, `name`, `rect`.
  - ext: `control_type_id`, `automation_id`, `class`, `ancestor_names`.
  - Otherwise the string `timeout|hung|unavailable|stale|outside_approval`, or null for a ready read without an element.
- **element**
  - PDF: `kind`, `trigger` (`pointer_down`|`pointer_rest`), `pos` {x,y}, `uia`.
  - ext: `window`, `geometry`, `action_t_us`, `post_action`, `action_to_completion_us`, `dpi`, `dpi_awareness`, `approved`, `request_id`, `generation`, `requested_t_us`, `started_t_us`, `completed_t_us`.
  - Unapproved: `{kind:"element", approved:false}`.
- **pause / resume:** `reason` (PDF, the first of `reasons`, null when empty), `reasons` (ext); the other fields are unchanged.
- **Unchanged:** `ime_state`, `caret`, `cursor`, `cursor_asset`, `keyboard_layout`, `app_busy`, `window_hint`, `app_hung*`, `display*`, `monitor_*`, `clipboard_change`, `pause`, `resume`, `discontinuity`, `recorder_control`, `recorder_control_gap`, `input_source_loss`, `input_source_coverage`, `gap`, `health`.
- Blind rows have moved to input. `context_source_gap` is now a `gap` row with `reason:"staging_pressure"`.

## stills.jsonl (one row per trigger)
- PDF: `kind` (`still`|`still_dropped`), `source_t_us`, `monitor`, `trigger`, `frame_obs_id`, `event_ref`, `file` (`images/<digest>.png`, relative to `raw/`, or null), `width`, `height`.
  - `seen_obs_id`: pre_motion and pre_action rows only, may be null.
  - `seen_file`: only when it differs from `file`.
- ext: `role` (`before`|`image`|`final_save_settled`), `trigger_t_us`, `reason`, `geometry`, `admission_epoch`, `scroll_device`, `settled_confirmed_t_us`, `adjustment`, `frame` {`id`, `source_t_us`, `recv_t_us`, `approval_generation`, `width`, `height`, `color`, `source_uncertainty_us`}, `element`, `outcome`, `save` {`id`, `kind`, `routing_basis`}.
  - Pre rows only: `seen_frame`, `seen_outcome`.
  - Fallback rows only: `fallback_reason`, `geometry_evidence`.
- `schema_version` is removed.

## frames.jsonl (no `kind`)
- PDF: `monitor`, `obs_id`, `source_t_us`, `recv_t_us`, `pts_us`, `encoded`, `segment`, `frame_in_segment`, `repeated`, `change`, `geometry_version`.
  - `recv_t_us` is the true capture receipt for a newly selected frame, otherwise the ordering receipt max(prev, e_k) (plan §6.5).
- ext: `slot`, `slot_decided_t_us`, `published_t_us`, `admission_epoch`, `reason`, `pts_100ns`, `duration_100ns`, `frame_id`, `capture_recv_t_us`, `geometry`, `late`.
- Optional fields are omitted when null.
