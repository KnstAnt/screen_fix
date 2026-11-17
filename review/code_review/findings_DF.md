# Review findings, areas D and F (verbatim essentials)

## D: context and the elevated core
- **D1** (med-high, bugfix §6.2 row 18 / C3), context.rs:360-362.
  - **Problem:** `elevated_invalidation` returns `Err(Failure::Witness)` on any ring loss, and input.rs:816-819 turns that into `fail_source`. While an elevated epoch has no open visit (activation/HeldPending/Refreshing, up to READINESS 5 s), the ring is not drained, so an overflow that lies entirely before `after` fails the source (F). The loss is also never ledgered.
  - **Fix:** note the loss as `LossReason::NotificationOverflow`. Return Witness only if `last >= after` (or treat that case as a revoke). Drain and count notifications while no visit is open, as `discard_paused_notifications` does.
- **D2** (med, bugfix WRITE_BUDGET/E3; plausible timing), peer.rs:201-204 with visit.rs:496-498, 536, 505-523.
  - **Problem:** required held reads (`query`) fail with Transport when QUERY_QUEUE (8) is full. Optional reads (`try_query`, up to 32 outstanding) fill the same queue.
  - **Fix:** reserve slots for required reads: `try_query` refuses at QUEUE-2 (an atomic count decremented by the writer), or `query()` waits up to WRITE_BUDGET.
- **D3** (med, bugfix E4; plausible race), selector.rs:123-125.
  - **Problem:** sleep classification uses `marks.resume.is_some_and(|r| host_live <= r)`. The resume mark may not be set yet when the dead peer is seen, giving F instead of G.
  - **Fix:** store `marks.sequence` with `host_live`; `sequence > stored` means slept.
- **D4** (med-low, bugfix I7 / doc; confirm the intended stream), context.rs:944-951, elevated.rs:211-221, 475-501, context.rs:757-765.
  - **Problem:** the elevated `take_confirmed_text` returns an empty Vec, and its commit losses go to the context stream with `producer: null`. The ordinary path sends them as input losses with `LossProducer::TextEdit`.
  - **Fix:** return elevated commit losses as `LossEvidence` with `TextEdit`.
- **D5** (low-med, bugfix C3), context.rs:373-376, elevated.rs:79-80.
  - **Problem:** at FUTURE_NOTICE_LIMIT, `elevated_invalidation` drops a notice uncounted; `poll` and `queue_window_notice` count the same case. `poll_elevated_queries` drops stale future notices silently.
  - **Fix:** `losses.note(NotificationOverflow, 1, receipt, receipt)`; for stale window-kind notices, emit the timed_out row or count them.
- **D6** (low-med, bugfix C4 trailing edge), elevated.rs:564-571 vs context.rs:669-674, 693-706, 738-749.
  - **Problem:** the elevated path re-implements `location_limited` and `rate_location` inline and drops rate-limited notices without the dirty/trailing re-read.
  - **Fix:** use the helpers, push to `location_dirty`, and run the trailing loop in `poll_elevated_queries` with `Query::Window`.
- **D7** (low, none), duplication between context.rs and context/elevated.rs. Duplicated pieces, by location:

  | Duplicate | Locations |
  |---|---|
  | caret row | 624-633 / 176-185 |
  | cursor staging cap 16 | 717-727 / 188-198 / 1121-1125 |
  | clipboard staging | 887-898 / 377-386 |
  | classification + busy row | 602-609 / 153-163 |
  | notice → window kind | 658-668 / `window_notice_kind` |
  | `approve_receipt` blocks | 918-924 / 935-941 |
  | `fixture_save_proof` / `checkpoint_context` preconditions | 818-819 / 835-836 |
  | elevated focus predicate | `Context::confirm_focus` 1024-1028 / visit.rs `focus_current` 566-573 |
  | `elapsed_deadline` / `add_time` | elevated.rs:664-667 / visit.rs:579-583 |
  | notice scans | selector.rs:964-984 |

  `Context::poll` is about 315 lines.
  - **Fix:** helpers plus a split of `poll`.
- **D8** (low, none), magic numbers.
  - The 16 ms hook uncertainty is written twice in different units (context.rs:1052 ticks, :1090 µs).
  - 750 ms appears at visit.rs:124, 399 and as selector `CLEANUP`.
  - 250 ms appears at selector.rs:1085 and context.rs:1268.
  - Capacity literals 64/32/16/8/4; µs second literals.
  - **Fix:** named consts, one HOOK_UNCERTAINTY and one CLEANUP_BOUND.
- **D9** (low, none), context.rs:173-178, 556-567, 1247-1270.
  - **Problem:** `Suppression.reason` is `&'static str`; `access()` returns `Result<ForegroundAccess, bool>`.
  - **Fix:** a `SuppressionReason` enum (snake_case serde, same values) and an `AccessUnknown` enum.
- **D10** (low, none), encapsulation.
  - launcher `Ticket`/`LaunchStatus`/`start`/`poll`/`cancel` are pub(crate) but used only by the selector; make them pub(super), and `cancel` private.
  - `Host.peer` is pub(super) (visit.rs:21), so the selector bypasses Host; add `Host::send_stop`/`next_status`/`next_observation`/`next_input`.
  - Selector keeps `session_status` and also takes `session` params; drop the redundant params.
- **D11** (low, none), elevated.rs:128, 184, 272, 574.
  - **Problem:** a clock-conversion error maps to `Failure::Witness`, and a `pointer_query` ProbeError to `Failure::Supervisor`.
  - **Fix:** attribute correctly.
- **D12** (low, none), ingress.rs:36, 50, 192-203.
  - **Problem:** `context_position: [u64; 5]` starts at `[0,1,0,0,0]` without a comment; the producer → index mapping is duplicated with collector outbox.rs:148.
  - **Fix:** add `ContextProducer::ALL`/`index()` in the protocol.
- **D13** (low, change; plausible), peer.rs:156-157.
  - **Problem:** an undecodable context-lane payload is fatal (Protocol), while ELEVATED §6.1 "Lane receipt" reads as lane-generic.
  - **Fix:** count it as a context loss, or state that §6.1 covers the input lane only.

## F: session, protocol, UI (no high or medium findings)
- **F1** (low, bugfix), session/windows.rs:389-403, 1594-1602, 1477.
  - **Problem:** `Control::submit` accepts `Pause(User)` in Starting, which stalls the countdown. The hotkey path returns PauseUnavailable.
  - **Fix:** ignore `Pause(User)` unless Recording/Paused; `submit` returns PauseUnavailable.
- **F2** (low, bugfix A2), fault.rs:103-112.
  - **Problem:** dedupe scans only retained reports, so above the cap of 32 a re-reported omitted cause is counted twice.
  - **Fix:** a bounded set of omitted causes, or rename the field to "omitted reports".
- **F3** (low, none), session/windows.rs:1-2, 199-200: the module doc says WorkerSession is read-only, which is wrong; the stop latch doc lists only `request_stop`.
- **F4** (low, none).
  - The closing predicate `stop || session_end_started` is open-coded about 8 times (windows.rs:109-110, 136, 488, 494, 528-529, 1581, 1793).
  - The stop latch is written in 6 places (407, 675, 687, 1332, 1582, 1672).
  - **Fix:** `Lifecycle::closing()` and `latch_stop(wake)`.
- **F5** (low, none).
  - The required-registrations predicate appears 4 times (ipc.rs:605, 612-614; runtime.rs:328-330, 380-381; input.rs:208; selector.rs:1017).
  - The environment flags check appears twice.
  - **Fix:** `InputRegistration::required_ready()` and `EnvironmentState::valid()`.
- **F6** (low, none), runtime.rs:374-375: `input_valid` clones the sample to call `validate`. Extract a `validate_input(&CollectedInput)`.
- **F7** (low, none), config.rs:8, 10: private copies of `MAX_APPROVED_EXECUTABLES` and the 4096 key bytes. Use the protocol constants and add `MAX_APPROVAL_KEY_BYTES`.
- **F8** (low, none).
  - The VK mapping `0x6f + function` is re-implemented in hotkeys.rs:242 (`virtual_keys()` exists).
  - The F12 special case is hard-coded (0x7b in hotkeys.rs, ==12 in ui/app.rs:878).
  - **Fix:** `VK_F1`, `virtual_key(action)` and `function_of(vk)`.
- **F9** (low, none), environment.rs:60-64, 357-367: hand-declared Win32 items exist in windows-sys (WTS_*, PBT_*, DEVICE_NOTIFY_CALLBACK, Register/UnregisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS). Enable `Win32_System_Power`.
- **F10** (low, none).
  - windows.rs:1228, 1406-1407: `(pause_producer_count-2)/2` reverses the encoding of the monitor count.
  - The "20 GB" literal appears in messages (disk.rs:45, windows.rs:1591); derive it from START_MINIMUM.
- **F11** (low, none).
  - `coordinate` is about 265 lines (windows.rs:1312-1578).
  - `run` polls `is_finished` with `recv_timeout(25 ms)`. **Fix:** wake on worker exit.
- **F12** (low, none), ui/app.rs:657: the UI searches the error text for "ms-settings:optionalfeatures". Use a typed remedy on CaptureError.
- **F13** (low, none), ui/app.rs:231-238: `request_repaint_after(40 ms)` while idle only to poll hotkeys. Use a repaint notifier.
- **F14** (low, none), error.rs:34-37: `CaptureError` has pub `kind`/`operation`/`native_code` duplicating `failure`. Make them pub(crate) or derive them.
- **F15** (low, none), protocol elevated.rs:136-145 vs 182-190: `UiaStatus` = Ready + ReadUnavailable, with two hand-written mappings (elevated runtime/context.rs:412-415, outcome.rs:181-189). Use `Result<(), ReadUnavailable>`.
- **F16** (low, change), time.rs: `scale_delta` truncates while `wgc_offset` uses `div_euclid`, so pre-origin times differ by up to 1 µs. Use `div_euclid` in `scale_delta`, or fix the comment.
- **F17** (low, none).
  - windows.rs:668-674: `fail_source`'s `admitted:false` is overwritten.
  - windows.rs:116-118: `with_closed_phase` upgrades the Weak twice.
