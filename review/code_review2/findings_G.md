# Audit findings, area G: elevated collector crate (`core/capture/elevated/**`), round 2

**Scope.** All 28 files of the crate, about 6.2k lines. Callers and dependencies were read as well: core `elevated/*`, `session.rs`, `clock.rs`, `context*`, `deploy/windows/release_resource.rs`, protocol `time.rs`, `ipc/elevated/runtime.rs`, and native-observation.

Round-1 findings G1–G18 are not repeated.

**Counts:** high 0, medium 3, low 14. One finding (G6) is a `change`; all others are `none`. G11 and G16 are PLAUSIBLE.

1. **G1** · medium · Architecture / DRY
   - **Where:** `runtime/owner.rs:93-245`. The closures are at 108-141, 145-151, 154-161, 165-192 and 197-229.
   - **Problem:**
     - One function configures the lanes, awaits the witness, spawns 5 threads as inline closures, builds `Owner`, and tears everything down.
     - Three of the closures hold domain rules: the urgent Stop/Revoke/Environment path, coverage credit and gap loss, and the egress gate.
     - The input and context egress loops are identical: batch → verify_once → qualify_egress → send_scoped → credit.
     - The urgent path restates the `before_qpc <= now` predicates of `handle` without the `.0 > 0` guard.
   - **Fix:**
     - Give the threads named functions: `control_reader`, `context_reader`, `input_monitor`, `input_egress` and `context_egress`, in a new `runtime/egress.rs`.
     - Add one generic loop, `egress<T>(queue, writer, qualify, on_sent)`, used by both egress functions.
     - Add `urgent_due(command, now)` and use it in both places.
   - **Size:** M · **Behaviour:** none

2. **G2** · medium · Architecture / KISS
   - **Where:** `runtime/owner.rs`. `handle` is at 281-439; the Prepare arm at 297-360, with teardown at 343-358; the resync fields at 373-374, 394-395, 459-460 and 632-633; `Owner.frequency` at 63.
   - **Problem:**
     - `handle` is a 160-line match. Its Prepare arm alone is 60 lines, and it tears down through a two-level nested match.
     - `resync_from` and `resync_expires` are two `Option`s that are always set together.
     - `Some(Instant::now() + READINESS)` is written 4 times.
     - `Owner.frequency` duplicates `scope.qpc_frequency`.
     - `Result<bool>` uses the bool to mean "stopped", which the type does not say.
   - **Fix:**
     - Give each match arm its own method: prepare, activate, refresh_focus, accept_resync, revoke, environment and stop.
     - Add a `VisitStart` guard that calls `grant.abandon` in `Drop` unless disarmed.
     - Add `struct Resync { from, expires }`.
     - Return `enum Flow { Continue, Stopped }` instead of the bool.
     - Remove `frequency`.
   - **Size:** M · **Behaviour:** none

3. **G3** · medium · DRY / idiomatic (same issue as F3/D7)
   - **Where:** "QPC + Duration" is computed by hand in grant.rs:95, 379-380; owner.rs:206, 663; context.rs:357, 646, 681-682, 732, 740; lifetime.rs:79-80, 112; input.rs:176. In addition:
     - lifetime.rs:38 has an inline `ticks` without an overflow check;
     - context.rs:742-744 has the inverse conversion;
     - core `elevated/visit.rs:593-597` (`add_time`) is a third copy;
     - the Teardown guard is written twice, at context.rs:357-358 and 646-647.
   - **Problem:** Every site repeats `x.0.checked_add(lifetime.ticks(d)?).ok_or(Supervisor)?`. The copies already differ: input.rs saturates, and core adds `.max(1)`.
   - **Fix:**
     - In protocol, add `QpcFrequency::ticks(Duration)` and `QpcTick::checked_after(freq, Duration)`.
     - In the collector, add `Lifetime::after(at, d)`, `instant_of(ticks)` and `teardown()`.
   - **Size:** M · **Behaviour:** none

4. **G4** · low · Architecture
   - **Where:**
     - `prepare` with 7 parameters: grant.rs:66-91; owner.rs:312-314; context.rs:248-257, 333-339.
     - Code reaching through the grant: owner.rs:206-207; context.rs:357, 681, 732; notices.rs:45-46, 59; input.rs:53, 176.
   - **Problem:** Per-connection constants (peer, witness View, Lifetime, stop handle, approved keys) are copied into every `Grant`, and other modules reach them through `grant.lifetime` and `grant.parent`.
   - **Fix:**
     - Add `struct Session { peer, witness, lifetime, stop, approved }` and pass it as `Arc<Session>`.
     - Change the signature to `Grant::prepare(request, lease, session)`.
     - Make the `Grant` fields private.
   - **Size:** M · **Behaviour:** none

5. **G5** · low · Architecture / DRY
   - **Where:** pipe.rs:72-79, 129, 147, 156, 168, 174; wire.rs:148-168; runtime.rs:223-231.
   - **Problem:**
     - The pipe's role is a runtime `bool`, and four methods check it at runtime.
     - Both frame readers repeat the same branch: the server reads one message, while the client reads the header, then the length, then the payload.
   - **Fix:**
     - Add `Pipe::read_frame(buf, header, payload_len_fn, deadline)`.
     - Make `is_server` private.
     - Optionally, split the type into `ServerPipe` and `ClientPipe`.
   - **Size:** M · **Behaviour:** none

6. **G6** · low · DRY (cross-crate)
   - **Where:** witness.rs:375-385 (`coarse_lower`); core `clock.rs:40-57`; protocol `time.rs:161-167`.
   - **Problem:** The collector maps a message tick to a lower QPC bound with its own hard-coded 16 ms and its own wrap handling, while the Recorder goes through `MessageTickAnchor`. The two sides of the trust boundary can drift apart.
   - **Fix:** Move `MessageTickAnchor::qpc_interval(freq, CoarseTime)` into protocol, and keep the collector's extra `elapsed <= 1 s` filter.
   - **Size:** S
   - **Behaviour:** `change` (the bound moves earlier by the bracket width plus one tick). It becomes `none` if only `tick_delta_ms` and the uncertainty constant are shared.

7. **G7** · low · DRY
   - **Where:** grant.rs: the snapshot predicate at 180-188, 278-279, 460-462, 497-498; live focus at 152, 340-341, 349, 408, 459; `Focus` reset at 293, 427.
   - **Problem:** The snapshot predicate appears 4 times with small differences, and `binding == visit && abort.check().is_ok()` appears 5 times.
   - **Fix:** Add `Snapshot::binds(visit, window)`, `Focus::live(visit)` and `Focus::new(binding, native)`. Keep the strictness exactly as it is.
   - **Size:** S · **Behaviour:** none

8. **G8** · low · DRY (cross-crate)
   - **Where:** context.rs:722-725; notices.rs:71; grant.rs:157-159; core `context.rs:430`, `context/elevated.rs:535`, `context/frame_policy.rs:96`.
   - **Problem:** A `WindowProcess` is built from a `WindowInstance` field by field at 6 sites, although the reverse `From` already exists (native-observation `identity.rs:52`).
   - **Fix:** Add `From<WindowInstance> for WindowProcess`.
   - **Size:** S · **Behaviour:** none

9. **G9** · low · DRY
   - **Where:** context.rs:217-230 (`read_times`); outbox.rs:170-187 (`producer_and_time`).
   - **Problem:** Two matches over `ContextEvidence` pull out the same header. The `_ => (None, None)` arm would silently hide a new variant.
   - **Fix:** Add one exhaustive `read_span(&ContextEvidence)`.
   - **Size:** S · **Behaviour:** none

10. **G10** · low · Architecture / idiomatic
    - **Where:** context.rs:400-487 (`Worker::uia`); 774-775 (tuples); 447-452 and 639-641 (reset).
    - **Problem:**
      - `Worker::uia` is 90 lines covering binding, budget, subscription, read, outcome, publish and re-arming commits.
      - The subscriptions are positional tuples.
      - The reset is written twice.
    - **Fix:** Add `struct Subscription { read, focus, revision }`, `Uia::drop_subscription()`, `uia_outcome` and `arm_commits`.
    - **Size:** M · **Behaviour:** none

11. **G11** · low · Architecture / idiomatic · PLAUSIBLE (overlaps F8)
    - **Where:** win.rs:12-89; core `session.rs:119-153`, `226-250`.
    - **Problem:**
      - `Failure` doubles as the Recorder's vocabulary. Six of its variants are produced only in core.
      - Core maps it 1:1 onto `SourceFailureReason`, so a new reason means 5 edits.
      - Its 35 `Display` strings duplicate `guidance()`, and no consumer of them was found.
    - **Fix:** Trim `Display`, and implement `From<Failure> for SourceFailureReason` next to `guidance`. Do not change any serialized names.
    - **Confirm by:** grep the UI and dev-log for `{}` formatting of `Failure`.
    - **Size:** S · **Behaviour:** none

12. **G12** · low · DRY (binary contract)
    - **Where:** trust/signature.rs:57-92 (roles 1/2/3, resource 271, RT_RCDATA, the 52-byte layout, the header `[1, role, 0, 0]`); deploy/windows/release_resource.rs:48-63; trust.rs:30-32.
    - **Problem:** The resource is written by the build script and read by the collector, but the two share no constants. A drift shows up only on a signed build.
    - **Fix:** Add `deploy/windows/release_format.rs` and include it in both through `#[path]`.
    - **Size:** S · **Behaviour:** none

13. **G13** · low · KISS / architecture
    - **Where:** deadline.rs:51-59; literals at collector.rs:49, recorder.rs:59, runtime.rs:174, runtime.rs:218, owner.rs:155.
    - **Problem:** `Deadline` has 6 public fields and is built as a full literal at 5 sites. `grant` and `scope_current` are always set together.
    - **Fix:** Add `Deadline::new(at, expired, cancel)`, `.peer()` and `.scoped(cancel, fn)`. Merge the two scope fields, and make the fields private.
    - **Size:** S · **Behaviour:** none

14. **G14** · low · Debt / magic numbers
    - **Where:**
      - 4 ms poll: owner.rs:29, context.rs:33, grant.rs:540;
      - notices.rs:45-46 (64 ms, 500 ms) and 114 (10 ms);
      - witness.rs:381-382 (1000 ms, 16 ms);
      - 25 ms: deadline.rs:12 and collector.rs:24;
      - owner.rs:741 (24 h).
    - **Problem:** Timing budgets are scattered. It is unclear whether the notice stall should equal `ESSENTIAL_PROGRESS`; both are 500 ms.
    - **Fix:** Collect them in `runtime/timing.rs`.
    - **Size:** S · **Behaviour:** none

15. **G15** · low · Architecture / idiomatic
    - **Where:** grant.rs:66-70 (`pub(super)` fields); 68 vs 250 (a field and a method both named `arm`); 416 (a 4-tuple); the `(Revocation, QpcTick)` pair at grant.rs:55, 163 and owner.rs:291, 680.
    - **Fix:** Rename the field to a private `armed_qpc` and add an accessor. Add `struct Reopened` and `struct Cut { reason, before }`.
    - **Size:** M · **Behaviour:** none

16. **G16** · low · State in booleans · PLAUSIBLE
    - **Where:** grant.rs:80 (`input_paused`); read at 295, 333, 387, 411, 417, 429, 434.
    - **Problem:** The state "Active, but input lost and waiting to reopen" is an `AtomicBool` that sits beside `Phase`, so every reader combines the two.
    - **Fix:**
      - Add `Phase::InputLost`, reached by `transition(Active, InputLost)`.
      - `reopen_input` moves InputLost → Opening.
      - `suspend_focus` accepts both Active and InputLost.
    - **Confirm by:** check that nothing depends on the order of setting the flag and cancelling the abort.
    - **Size:** M · **Behaviour:** none

17. **G17** · low · DRY (FFI)
    - **Where:**
      - `GetSecurityInfo` + `LocalAllocation`: pipe/security.rs:63-74 and trust/permissions.rs:27-36;
      - 32768-unit buffers: trust.rs:134-141, 229-233, 244-252 and process.rs:128-137;
      - `wide()` without the NUL: trust.rs:173-175, 256-257, 299-300;
      - manual `LocalFree`: security.rs:132-142.
    - **Problem:** The same FFI patterns are repeated, each with a slightly different bounds check.
    - **Fix:** Add `win::read_wide`, `win::wide_no_nul` and `win::acl::security_info`, and use `LocalAllocation` in `sid_string`.
    - **Size:** S · **Behaviour:** none

## Assessment
The crate is healthy:
- ownership is explicit: one Lane per direction, one Grant per visit, and an `Arc<Cancellation>` fan-out;
- `unsafe` stays local;
- the round-1 fixes landed cleanly;
- there are no TODOs, no `unwrap` on fallible paths, and no dev code on production paths.

Top debts:
1. The god functions in the owner runtime (G1, G2).
2. QPC/Duration arithmetic repeated at about 12 sites and in core (G3).
3. Per-connection state threaded through every Grant (G4, G15).

Fine as is: the overlapped pipe I/O, wire bootstrap and HMAC, `secret.rs`, token verification, the trust paths, the supervisor, and `AtomicState`.
