# Phase 5 brief: event-driven waits and deferred races (round 2)

**Repo:** `S:\myProgr\proj\rust\screen_fix`. Windows only.
**Baseline:** commit `e7fa79f` (phases 1–4 committed).
**Background:** read `review/code_review2/AUDIT_SUMMARY.md` (theme T5) and findings X7, X8 in `review/code_review2/findings_X.md`.

## Goal
Remove busy polling where a blocking primitive fits, without changing what the code decides.

Today the code polls in two places:
- the input pause/stop drains, the video waits and the stills drain use `park_timeout(2–5 ms)`, so they spin at 200–500 Hz;
- the environment owner wakes every 10 ms only to check for stop.

## Items
1. **X8: wait on Facts changes instead of fixed-period polling.**
   - Where the condition lives in controller `Facts`, wait on `Lifecycle::changed` with `wait_timeout_while`. Examples: pause prefix/paused video/input prefix in the input pause drain, video waits for admission close, the `finish_stills` drain waiting for `input_prefix_closed`.
   - Add a small helper, for example `WorkerSession::wait_changed(deadline, predicate)` or `poll_until(until, interval, step)`. It also composes the "min(local, stop/segment deadline)" bound that is currently built inline at about 4 sites.
   - **Safety rule:**
     - keep a timeout fallback equal to today's poll period or a small multiple of it, so a missed notify can only delay the wakeup, never deadlock or change a decision;
     - add `notify_all` at every Facts write that a waiter depends on. Find every writer with grep.
   - Where the waiter must also poll a non-Facts source, keep a bounded `park_timeout`, or unpark the waiting thread from the producer. That covers the helper pipe, `FlushOutcome` receivers and journal ingress. Prefer the smaller change.
2. **X7: the environment owner.**
   - It drains `PeekMessage` and then sleeps in `recv_timeout(10 ms)`. Make it wait on messages or a stop event: use `native_observation::wake::WakeEvent::wait_messages` with a bounded timeout of 100 ms or less.
   - The stop path must set the event.
   - Session-end/power notifications must still be handled in the same order.
3. **Deferred race (P3 from phase 1): `native_observation::owner::spawn`.**
   - An owner can call `ready()` after the starter's `recv_timeout` has timed out but before the receiver is dropped. The thread then believes it started and keeps serving, while the starter returns Timeout. For the clipboard owner (`join_on_failure == false`) nothing stops it.
   - Fix: add an `abandoned: AtomicBool` (shared through an `Arc`) that the starter sets before dropping the receiver. `ready()` returns false if it is set after a successful `try_send`, or check before and after. A thread that gets false from `ready()` must end without serving, which today's contract already says.
4. **Deferred P3: `os::process_image_names` is unbounded.** It walks the whole snapshot and only afterwards rejects more than 16 384 entries.
   - Restore the early stop at the cap for the settings caller. A walk with more than 16 384 entries must still return `None`, as today: stop at cap+1 and report "over".
   - Preflight semantics stay unchanged.

## Rules
- **Behaviour.**
  - Decisions, ordering of journal rows and side effects, deadlines and fault classes must not change. Only wake latency and CPU use may change.
  - No golden-test expectation may change.
  - Never weaken a privacy, approval or trust check.
- **Waits.**
  - Every new blocking wait keeps a bounded timeout as a safety net.
  - No lock may be held while waiting on another thread's progress, except the Condvar's own mutex.
  - Watch lock ordering. The `Facts` mutex must never be taken while another lock is held, if that lock is taken under `Facts` elsewhere.
- **Code.** Minimal, idiomatic code with LF line endings and UTF-8.
- **Commands.**
  - At the end, run `cargo check --workspace --all-targets`, `-p screen-fix-native-observation --features context`, `-p screen-fix --features fixture-save` and `-p screen-fix --no-default-features`. Each must finish with 0 errors and 0 warnings.
  - Do not run tests, clippy, fmt or the app.
  - Git is read-only.
- **Report.** The harness forbids writing report files, so put the report in your final message.

## Report (final message, English, at most ~500 words)
- **Per item:** done, partly or skipped.
- **Each converted wait site:** what it now waits on, its fallback timeout, and which writers notify it.
- **Lost-wakeup analysis** for each site.
- **Cargo results.**
