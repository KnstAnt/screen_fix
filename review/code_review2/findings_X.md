# Findings X: cross-cutting architecture

## Crate graph
- `ui -> core -> {protocol, elevated, native-observation[context]}`
- `elevated -> {protocol, native-observation[context]}`
- `input-helper -> {protocol, native-observation}`

The graph has no cycles, and the dependency directions are sound. The problems are inside `core`.

## Largest files

| File | Lines | Verdict |
|---|---|---|
| session/windows.rs | 2231 | Split it (X1). |
| video.rs | 1913 | The frames merge (84-460) is a separate concept from per-monitor `Video` (636-1754, 45 fields). Move it to `video/merge.rs`. |
| input.rs | 1797 | Move out `Pump::pause`/`stop` (482-832), `poll_elevated` (920-1165), and the row/envelope helpers (1643+). |
| context.rs | 1663 | `Context` has 48 fields and about 70 methods in a single impl (270-1517). |
| stills.rs | 1651 | `impl Stills` is about 990 lines. |
| gate.rs / receiver.rs | 1430 / 1427 | One concept each, but neither uses Windows APIs (X3). |
| metadata.rs / journal.rs | 1233 / 1050 | Fine as is. |

Counts: high 2, medium 7, low 9.

## Findings

1. **X1** · high · Architecture · L · none
   - **Where:** session/windows.rs:216-262 (`Facts`), 286-300 (`Lifecycle`), 444 and 512-1191 (`WorkerSession`, 89 `pub(crate)` methods).
   - **Problem:** A single mutex-guarded `Facts` holds source selection, pause state, health/fault reports, fixture saves, disk, hotkeys and clock anchors. Every owner thread gets the whole facade. Mapping the callers shows three separate groups:
     - about 25 source methods, called only from `elevated/selector.rs` and `input.rs`;
     - about 20 report getters, called only from `platform/windows.rs`;
     - the admit/pause/health methods, called by every producer.
   - **Fix:**
     - Split `Facts` into `SourceFacts`, `PauseFacts` and `ReportFacts`, still under the same `Mutex`.
     - Expose three facets: `SourceControl`, `ProducerSession` and `AttemptReport`.
     - Move the code to `session/windows/{facts,worker,controller}.rs`.

2. **X2** · medium · Layering · M · none
   - **Where:**
     - session/windows.rs:33 and environment.rs:51 import `platform::windows::{ProbeError, clock, hotkeys, preflight, unavailable, on_input_desktop}`.
     - platform/mod.rs:96 passes `windows::record_attempt` to `session::windows::spawn`.
     - About 20 platform imports of `session::windows::{WorkerSession, Receipt, PausedVideo, InputFailure, ...}`.
     - session/windows.rs:116-123, 696 and 726 use `screen_fix_elevated::Failure`.
   - **Problem:** `session::windows` and `platform::windows` depend on each other. `session/` is not the platform-neutral domain its name implies.
   - **Fix:** Move `session/windows*` (including `disk.rs` and `environment.rs`) to `platform/windows/controller/`. Keep `session.rs`, `fault.rs` and `hotkey.rs` as the neutral API.

3. **X3** · medium · Layering · L (incremental moves) · none
   - **Where:**
     - These files use no Win32 at all: input/hid.rs (422), hid/touchpad.rs, hook_health.rs, blind.rs, devices.rs, slots.rs (296), context/frame_policy.rs (227), still_time.rs.
     - gate.rs (1430) uses Win32 only for `BCryptGenRandom` (:1354).
     - platform/windows.rs declares 30 flat sibling modules.
   - **Problem:** Domain rules live in the platform layer, although `capture/input` exists for exactly this. The flat module list also hides the real subsystems.
   - **Fix:**
     - Move these files to `capture::input::{hid, touchpad, blind, devices, hook_health}` and `capture::video::slots`.
     - Inject the reference key into `Gate::new` so that gate.rs can move too. Do this last; the gate still depends on `Context`.
     - Group the rest of the platform modules into `video/`, `stills/` and `capture/` submodules.

4. **X4** · medium · Layering · M · none
   - **Where:** platform/windows.rs:221-450 (`record_with_disk`).
   - **Problem:** About 230 lines of package rules sit in the platform layer, mixed with owner shutdown. They decide:
     - which health rows join the final context rows;
     - the input blind rows (INP-13);
     - `omitted_health` accounting;
     - where the `attempt_failure` row goes.
   - **Fix:** Add `package::finalize::FinalRows::collect(&AttemptReport, prefix)`. `record_with_disk` keeps only the joins and the `finish_with` calls.

5. **X5** · high · Architecture / DRY · M for envelope + stills, L for all rows · none (compare the bytes before and after)
   - **Where:**
     - About 200 `json!` sites: gate.rs 65, hid.rs 25, input.rs 22, context.rs 22, context/elevated.rs 16, queries.rs 15, and others.
     - The envelope (`recv_t_us`, `source_t_us`, `source_uncertainty_us`, `tick_ms`, `tick_uncertainty_us`, `late`, `admission_epoch`) is written at input.rs:1785-1797, gate.rs:978-995, journal.rs:585-586, journal.rs:1015-1016 and context.rs:1517.
     - The still row is built three ways: stills.rs:457-480, stills.rs:1638-1651, and still_rows.rs:99-108 (which nulls fields from a list of strings).
   - **Problem:** The fixed serialized contract has no single typed definition. A typo or a missing field compiles, and only a reader notices.
   - **Fix:**
     - Add a typed `RowEnvelope` built from `RecordTime` plus the epoch.
     - Then add a typed `StillRow` with `dropped(reason)` and `fallback(..)`.
     - Input and context rows can follow later.
     - Rows can keep going through `to_value`. `preserve_order` is off, so keys stay sorted and the bytes are unchanged.

6. **X6** · medium · DRY / idiomatic · M · none
   - **Where:**
     - `.map_err(package_journal_error)` 36 times; `.map_err(clock_error)` 59 times.
     - The 6-field `ProbeError` literal at platform/windows.rs:149, 153, 160 and 484, and at input_helper.rs:369.
     - The 7-field `CaptureError` literal 5 times in error.rs:44-115.
   - **Problem:** About 95 conversions are written by hand, and adding a field to either error type means editing every literal.
   - **Fix:**
     - `impl From<JournalError> for ProbeError` and `impl From<ClockRangeError> for ProbeError`.
     - Private constructors `ProbeError::new(stage)` and `CaptureError::new(kind, failure, message)`.

7. **X7** · medium · DRY / concurrency · M · none
   - **Where:**
     - The same startup handshake (spawn, open, `try_send(Result)`, `recv_timeout(2 s)`) appears at events.rs:186-250, hooks.rs:87-93, raw_loop.rs:74-87, clipboard.rs:124-177 and hotkeys.rs:171-188.
     - There are four styles of message pump:
       - a wait on a wake event (clipboard:171, hotkeys:395, events:242);
       - PeekMessage plus `recv_timeout(10 ms)` (environment.rs:252-262);
       - a blocking `GetMessageW` (raw_loop, hooks);
       - witness.rs in the elevated crate.
   - **Problem:** Seven native owner threads each implement startup, stop and the pump again. The environment owner wakes 100 times a second just to check for stop.
   - **Fix:**
     - Add `native_observation::owner::{spawn_owner, MessagePump}`.
     - Make the environment owner wait for messages or a stop event, with a 100 ms timeout.

8. **X8** · medium · KISS / concurrency · M · none
   - **Where:**
     - `park_timeout(2-5 ms)` loops at input.rs:498-520, 566-576, 609-633, 703-711 and 1356-1377; video.rs:776-790, 1365-1377, 1594-1600 and 1680-1686; monitor.rs:703-714.
     - The deadline `min(local, stop/segment)` is built inline at input.rs:705, input.rs:1372, monitor.rs:703 and video.rs:1370.
   - **Problem:** Nothing unparks the input thread (`unpark` is called only in stills, wgc and environment), so the pause and stop drains spin at 200-500 Hz. Some of these loops wait on `Facts`, even though the `Lifecycle::changed` Condvar exists and `wait_for_close` already uses it.
   - **Fix:**
     - Add `poll_until(until, interval, step)` and `WorkerSession::bounded(deadline)`.
     - Where the condition is in `Facts`, use `changed.wait_timeout_while`.
     - PLAUSIBLE: this needs `notify_all` on changes to `paused_video` and `pause_prefix`. Only 3 sites notify today.

9. **X9** · low · DRY / missing type · M · none
   - **Where:**
     - The `(count, first, last)` span is passed as a tuple: `LossCounter::take` (native-observation lib.rs:66), events.rs:140, clipboard.rs:184, context/events.rs:24, context.rs:226-240, gate.rs:217.
     - A hand-written min/max merge appears at still_rows.rs:39-55 and receiver.rs:1120, and there are about 7 more typed merges.
   - **Fix:** Add `protocol::Span<T>` / `LossSpan`.

10. **X10** · low · DRY · S · none
    - **Where:**
      - The bracket-ticks to µs ceiling, at clock.rs:81 and context.rs:1660.
      - `clipboard_change_after_visit` (context.rs:1654-1662) re-implements `MessageTickEstimate`.
      - The slot ceiling, at session/windows.rs:501 and slots.rs:54.
      - input.rs:1159 floors ticks/µs inline.
    - **Fix:** Add `MessageTickEstimate::follows` and `QpcFrequency::ticks_to_us_ceil`, and reuse `slots::Timeline`.

11. **X11** · low · DRY · S · none
    - **Where:** five static single-flight leases: session/windows.rs:69-84, identity.rs:18-24, launcher.rs:32-46, selector.rs:31 and 1063-1072, hooks.rs:52-70.
    - **Fix:** Add `StaticLease::claim(&'static AtomicBool)`.

12. **X12** · low · DRY / idiomatic · S · none
    - **Where:** three COM `Apartment` guards: platform/windows.rs:181-195, launcher.rs:261-273, uia.rs:113-126.
    - **Problem:** Only the uia.rs guard is `!Send`. The platform guard enforces "stays on its thread" only through a comment.
    - **Fix:** Use a single `ComApartment` holding `PhantomData<*const ()>`.

13. **X13** · low · Layering / DRY · M · none
    - **Where:** session.rs:126-154 and 222-262 (a 1:1 mapping from the elevated `Failure` in win.rs:12-47); context.rs:443-445; input.rs:1089; preflight.rs:213-247.
    - **Problem:**
      - A new collector failure has to be added in 3 places.
      - Core also uses the collector's `Failure` for unrelated conditions. For example, a stopped notification loop becomes `Witness`.
    - **Fix:** Move `SourceFailureReason` to protocol (same serialized names) and add `Failure::reason()`.

14. **X14** · medium · Leaky platform boundary · M
    - **Where:** ui/src/app.rs has 58 `cfg(target_os = "windows")`; the core public API has 14 in platform/mod.rs, 12 in session.rs and 11 in error.rs.
    - **Problem:** elevated and native-observation compile only on Windows, and recording is Windows-only. The portability these `cfg`s pay for is never used.
    - **Fix (choose one):**
      - make the core API always present and return `unsupported` outside Windows, so the UI needs no `cfg` (`none`); or
      - declare the workspace Windows-only and drop the stubs and the Linux eframe dependencies (`change`).

15. **X15** · medium · Build · S · change
    - **Where:** ui/Cargo.toml:9-12 has `default = ["dev-log"]`; the release recipe (deploy/windows/README.md:5) enables `elevated-release` without `--no-default-features`.
    - **Problem:** Signed release builds write `dev.log` into every attempt, and nothing prevents it.
    - **Fix:** Add `#[cfg(all(feature = "elevated-release", feature = "dev-log"))] compile_error!(...)` in ui/src/main.rs, or remove `dev-log` from `default`.

16. **X16** · low · Build · S · none
    - **Where:** `fixture-save` gates only the UI (app.rs:63-65, 110-112, 601); `pub fn confirm_fixture_save` (session.rs:415) is always compiled.
    - **Fix:** Add a core `fixture-save` feature that ui forwards, and gate only the entry point. The routing behind it is plausibly the intended M1 save pipeline, so leave it ungated.

17. **X17** · low · Misplaced module · S · none
    - **Where:** protocol/src/ipc/shared.rs is a Windows file mapping with `unsafe`, in a crate documented as platform-independent.
    - **Fix:** Move it to `native_observation::channel`.

18. **X18** · low · Consistency · S · none
    - **Where:** raw `u64` instead of ID newtypes: `focus_generation` (visit.rs:30), `epoch` (owner.rs:67), `generation` (frame_policy.rs:23, 31).
    - **Fix:** Store the newtypes. Everything else is consistent; for example, all 29 `*_t_us` fields already use `SessionTimeUs`.

## Concurrency
A recording runs about 22 + 3N threads (N = monitors), plus 3 in the helper. Native thread affinity (hooks, message windows, COM) justifies these threads, so I would not merge any of them.

The debt is how the threads wait (X7, X8). WGC and stills are event-driven, but the drains and the environment owner poll every 2-10 ms. A single `Mutex<Facts>` is adequate; its problem is breadth, not contention.

## Assessment
Overall the code is in fair shape.

What is healthy:
- the crate graph has no cycles;
- `capture/` contains no Windows types;
- the time and ID newtypes are consistent.

What is debt:
- `session` and `platform` together form one tangled Windows runtime;
- the fixed journal contract is built from untyped JSON.

The top debts are X1, X5 and X3/X4.

Fine as is: the crate split, the `context` feature, the time and ID types, the single controller mutex, the number of threads, and the sizes of metadata.rs and journal.rs.

Tests: the workspace has none, by the user's decision (REMEDIATION_PLAN:704). X3 would make the pure logic cheap to test.
