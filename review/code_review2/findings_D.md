# Findings, area D: context and the elevated core (round 2)

Scope: `context.rs`, `context/**` and `elevated/**`. Round-1 items D1–D13 are already applied (confirmed by grep).

Totals: high 1, medium 3, low 13. Every finding has behaviour `none`. D14 is PLAUSIBLE.

1. **D1** · high · Architecture
   - **Where:** context.rs:270-326, 359-392, 1442-1513; context/elevated.rs:7, 86-88, 264-265, 117-119.
   - **Problem:** `Context` has about 53 fields and covers at least five separate concepts:
     - the visit timeline;
     - display routes and geometry;
     - clipboard staging;
     - the location rate limit;
     - the access cache.

     The elevated half is a second `impl` that does `use super::*` and writes private fields directly. `pause()` has 30 hand-written resets. Route and geometry invalidation is spread over 5 places.
   - **Fix:** split it into sub-structs in neighbouring files, each with its own reset:
     - `DisplayRoutes`: `refresh_displays`, `point_geometry`, `only_geometry`, `save_geometry`, `observe_bounds`;
     - `ClipboardStaging`;
     - `VisitTimeline`: `hook_receipt`, `prune_visits`, `focus_end`, `verified`, and the historical part of `approve_receipt`;
     - `LocationLimiter`.

     Replace the glob import with explicit imports.
   - **Size:** L · **Behaviour:** none

2. **D2** · medium · Architecture / DRY
   - **Where:** selector.rs:33-72, 710-883, 492-497 vs 716-721, 813-818 vs 945-950, 857-866 vs 953-966, 826-843.
   - **Problem:**
     - `Selector` has 27 fields mixing launch, the queue, cutoffs, blind tracking and sleep.
     - `poll_host` (about 175 lines) does four jobs.
     - The status restriction, the notice cutoff and the input drain are each copied 2–3 times, into `stop` and `drain_closed_lanes`.
     - The `ContextGap` merge is written twice inside one match.
   - **Fix:**
     - Add `ContextQueue { items, pressure, optional_dropped }` with `push`, `ready`, `deferred_read`, `earliest_notice` and `retire`.
     - Add `apply_status_restriction`, `drain_input` and `merge_context_gap`.
     - Split `poll_host` into `poll_status`, `poll_observations`, `poll_input` and `revoke_on_pressure`.
   - **Size:** M · **Behaviour:** none

3. **D3** · medium · Stringly typed
   - **Where:** context.rs:1365, 1380, 1544-1555, 841, 856, 869; context/elevated.rs:21, 200, 361, 533, 537, 586, 626, 634, 644, 647, 687.
   - **Problem:**
     - Window row kinds are passed as `&str`, and code branches on `kind == WINDOW_LOCATION` in 4 places.
     - `native_status` rows are assembled by hand at 6 sites, with different field sets.
   - **Fix:**
     - Add `enum WindowRowKind` (snake_case, same values) and `enum NativeStatus`.
     - Add a `native_status_row(kind, window, approved, status)` helper.
     - `window_event_kind` returns `Option<WindowRowKind>`.
   - **Size:** M · **Behaviour:** none

4. **D4** · medium · State machine in Options
   - **Where:** visit.rs:46-69, 133-142, 151-155, 326, 345-352, 370, 378, 412-418.
   - **Problem:**
     - `Grant.closing_before` is `Some` exactly when the phase is Closing.
     - `registration` duplicates `Phase::Armed(registration)`.
     - `focus` is `None` in some phases.

     The types do not guarantee these invariants, so the code re-checks them with `ok_or(Failure::Protocol)` at 4 sites.
   - **Fix:** use `Phase::Closing { before }` and keep one copy of `registration`. Optionally, make `focus` non-optional after Armed.
   - **Size:** M · **Behaviour:** none

5. **D5** · low · DRY
   - **Where:** context/elevated.rs:715-724 (`fresh_read`), queries.rs:578-585 (`remote_read`), visit.rs:466-469 and 481-486.
   - **Problem:** the read-timing rule `requested ≤ started ≤ completed ≤ at < deadline` is written 3 times, in different forms.
   - **Fix:** add `read_window<T>(&ReadEvidence<T>, at) -> Option<(start, end)>` in elevated/mod.rs. Keep each caller's inclusivity.
   - **Size:** S · **Behaviour:** none

6. **D6** · low · DRY
   - **Where:** context.rs:1654-1662 vs clock.rs:76-83; context.rs:473-474 vs elevated.rs:728-729.
   - **Problem:** `clipboard_change_after_visit` recomputes the message-tick estimate by hand, and the notice-interval chain is written twice.
   - **Fix:** add `MessageTickEstimate::not_before(start)` and delete `MICROS_PER_SECOND`. Add `notice_interval(notice, receipt)`.
   - **Size:** S · **Behaviour:** none

7. **D7** · low · DRY / idiomatic (overlaps F3)
   - **Where:** visit.rs:593-597, 525-528; peer.rs:46-47; context.rs:1220; elevated runtime lifetime.rs:101-104; input.rs:1159; session/windows.rs:1648, 2202; environment.rs:118.
   - **Problem:** about 9 hand-written Duration↔QPC conversions, with mixed rounding and `as` casts.
   - **Fix:** add `QpcFrequency::ticks(Duration)` and `micros(i64)`, and build `add_time` on top of them.
   - **Size:** M · **Behaviour:** none

8. **D8** · low · Missing type (overlaps H-11)
   - **Where:** context.rs:226-240; selector.rs:56, 973-981, 826-843; native-observation lib.rs:66; clipboard.rs:184; input.rs:964-969; elevated.rs:243.
   - **Problem:** loss spans appear as tuples in about 5 shapes, with field order varying between them. The gap-count formula is duplicated.
   - **Fix:** add `struct LossSpan { count, first, last }` with `note`/`merge`, returned by `LossCounter::take`. Add `ContextGap::count()`.
   - **Size:** M · **Behaviour:** none

9. **D9** · low · Missing types
   - **Where:** context.rs:441-480 with input.rs:929-945; selector.rs:39, 53-54, 64, 388, 409; queries.rs:87; context/elevated.rs:30.
   - **Problem:** `elevated_invalidation` returns `(QpcTick, bool)`. The Selector API returns 4- and 3-tuples. `helper_check`, `cached_pointer` (6 fields) and `ime_key` are also tuples.
   - **Fix:** add `enum Invalidation { Foreground(t), Focus(t) }`, plus `CoverageLossReport`, `Recovered`, `HelperCheck`, `CachedPointer` and `ImeKeyRead`.
   - **Size:** M · **Behaviour:** none

10. **D10** · low · DRY (workspace)
    - **Where:** selector.rs:1034; ingress.rs:71, 194; visit.rs:137, 413; input/gate.rs:392; session/windows.rs:1926, 2062.
    - **Problem:** `Some(slot.map_or(v, |o| o.min(v)))` appears 8 times.
    - **Fix:** a shared generic `keep_earlier`.
    - **Size:** S · **Behaviour:** none

11. **D11** · low · DRY (workspace)
    - **Where:** launcher.rs:32-46; selector.rs:31, 1061-1072; identity.rs:18-24, 97-99; session/windows.rs:69-83.
    - **Problem:** a single-owner latch with a Drop guard is written 4 times.
    - **Fix:** one `LatchGuard::claim(&'static AtomicBool)`. Keep the four permitted statics.
    - **Size:** S · **Behaviour:** none

12. **D12** · low · DRY / consistency
    - **Where:** settings.rs:83-121 vs preflight.rs:93-120; cursor.rs:429-474 vs still_png.rs:350-381.
    - **Problem:**
      - The Toolhelp process enumeration is written twice, with different handle guards.
      - The PNG pending→encode→publish flow is written twice. The cursor version uses `fs::rename` and `.pending`; the stills version uses `move_new` and `.png.pending`.
    - **Fix:** add `process_image_names()`, `write_png_rgba8_new()` and one shared publish helper.
    - **Size:** M · **Behaviour:** none

13. **D13** · low · DRY / KISS
    - **Where:** queries.rs:66-90, 269, 310, 321, 333, 370, 152, 236.
    - **Problem:**
      - The unavailable `Read` literal is written out 5 times.
      - Focus and pointer each use 4 parallel fields.
      - The magic number 500_000 appears twice.
    - **Fix:** add `Read::unavailable()` and merge it with `uia::Read::outcome`. Add `PendingQuery { request, sent, remote }` and named constants for the ages.
    - **Size:** M · **Behaviour:** none

14. **D14** · low · Layering / DRY · PLAUSIBLE
    - **Where:** frame_policy.rs:197-406, 220/232/238/274, 306-310 vs 346-350, 250-255 vs 295-299, 318.
    - **Problem:**
      - The pure admission rules live in platform/windows/context.
      - In the file itself: the lock is written 4 times, `try_read` twice, generation minting twice, and the freshness check in 4 forms.
      - The devlog string at 318 has extra spaces.
    - **Fix:**
      - Optionally move the rules to `capture/frame_policy.rs`, keyed by `(WindowId, ProcessInstance)`.
      - Add `approve`, `fresh`, `read` and `write` helpers.
      - Fix the string.
    - **Size:** M (move) / S (helpers) · **Behaviour:** none

15. **D15** · low · DRY / idiomatic
    - **Where:** visit.rs:501-519 vs 543-556, 531-536, 134-135, 385-386, 407-408, 189/213/267/308/523; selector.rs:500, 505.
    - **Problem:**
      - The held `ReadBinding` and the request-id allocation are each triplicated.
      - `retry` and `retry_after` are a pair of Options.
      - Imported types are still written with full paths.
    - **Fix:** add `held_read()` and `clear_retry()`, and use the short paths.
    - **Size:** S · **Behaviour:** none

16. **D16** · low · DRY
    - **Where:** ingress.rs:125-133 vs 146-148; context/elevated.rs:155-157 vs 258-260; hung.rs:137 vs 167, 161 vs elevated.rs:194.
    - **Problem:** small duplicates: the staging-loss init, owned-window removal, the ghost-confirmed row and the elevated coverage row.
    - **Fix:** add `pressure_at`, `forget_window` and row helpers.
    - **Size:** S · **Behaviour:** none

17. **D17** · low · Idiomatic
    - **Where:** launcher.rs:70-92, 175-178, 197-209.
    - **Problem:**
      - An `AtomicU8` with three `u8` constants is an enum in disguise.
      - `poll` destructures again after `mem::replace`, which needs an `unreachable!`.
    - **Fix:** `#[repr(u8)] enum Dispatch`, and `match mem::replace(..)` directly.
    - **Size:** S · **Behaviour:** none

## Assessment
The code is correctness-minded and well commented, and the round-1 fixes hold. The structure has not kept up.

Top debts:
- the god objects `Context` and `Selector` (D1, D2);
- string-typed window rows and statuses (D3);
- missing small value types: loss spans, invalidation, QPC conversions, Grant phases (D4, D7–D9).

Fine as they are: `peer.rs`/the lease, the ingress checks, `ime.rs`, `version.rs`, the `uia.rs` gate, `busy.rs`, the `hung.rs` scan, the launcher consent flow, and cursor decoding.
