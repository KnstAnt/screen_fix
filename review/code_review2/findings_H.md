# Findings, area H: native observation and input helper crates

Scope: `core/capture/native-observation/**` (about 4.6k lines) and `core/capture/input/**` (about 250 lines). The round-1 fixes are in place: E4, H2–H9 and H11–H13.

Counts: 0 high, 5 medium, 9 low. Every finding has behaviour `none`, except H-12, which becomes a `change` if the stricter check would reject a real token.

1. **H-1** · medium · Architecture (god function)
   - **Where:**
     - `raw_loop.rs:275-426` (`raw_loop`)
     - `:231-272` (`announce`, `announce_as`, `request`, `deliver`)
     - `:428-458`
     - `:460-466`
   - **Problem:** `raw_loop` does seven jobs:
     - the stop protocol, written twice (:302-306, :313-323)
     - scope re-inventory
     - WM_INPUT plus first-seen devices
     - device change
     - the timer
     - the environment
     - progress

     Device state is threaded through loose parameters: `&DeviceWorker`, `&mut DeviceIds`, `&mut u64`, `&mut Evidence`, `&mut RawWindow`. `announce` takes 7 parameters. The tuple `(DeviceDescriptor, Option<Vec<u16>>)` passes through 5 functions.
   - **Fix:**
     - Add `struct Devices { worker, ids, sequence }` with `announce`, `first_seen`, `removed`, `deliver` and `rescope`.
     - Add `struct ProbedDevice { descriptor, path }`.
     - Split the loop into `on_input`, `on_device_change`, `on_timer` and a single `close()`.
     - Add `default_cleanup(&MSG)` to replace the 3 inline `DefWindowProcW` calls.
   - **Size:** M · **Behaviour:** none

2. **H-2** · medium · DRY
   - **Where:** `input/src/windows.rs:120-166`, `raw_loop.rs:145-175`, `elevated/src/runtime/input.rs:119-166,217-235`.
   - **Problem:** Round 1 H8 extracted the primitives, but the sequence around them is still written three times: defer status → flush the producer's gaps → `drop_record` on refusal → send → `drop_record` on Full → `flush_pending`. The order already differs between the copies (elevated flushes losses before it defers status). The E3 rule, that a record never overtakes its producer's gap, is restated in every copy.
   - **Fix:**
     - Add `Backlog { losses, status }` in `loss.rs`, with `offer(record, send -> Result<Sent{Accepted, Full, Malformed}, E>)`, `flush(producers, send)` and `finish()`.
     - Each owner keeps only its own `send`. The elevated filter stays in front of `offer`.
   - **Size:** M · **Behaviour:** none (the order is equivalent, because `loss::producer` is `None` for status records)

3. **H-3** · medium · KISS (clever code)
   - **Where:** `input/hooks.rs:41-43,335-411,414-453,268,351`.
   - **Problem:**
     - A ring entry is `[u64; 11]` with sentinel values: `seq==0 && qpc==0` means Progress, and `seq==0` means Gap, with its fields shifted by one word.
     - `unpack` reads `first_sequence` from the variable named `qpc`, and so on down the shifted fields.
     - The layout is written in 4 places, and the gap bounds are a positional `[AtomicU64; 4]`.
     - Getting one word wrong would silently corrupt gap ranges (E3).
   - **Fix:**
     - Add a private `enum Entry { Event{..}, Gap{..}, Progress(i64) }` with `encode` and `decode` next to each other, using a tag word instead of sentinels.
     - Replace the positional array with named gap atomics.
     - Make `failed` an `AtomicBool`.
   - **Size:** M · **Behaviour:** none

4. **H-4** · medium · Architecture / KISS
   - **Where:** `uia.rs:204-368` (`read_inner`); checkpoints at :208-222, :233-241, :256-260, :316-321, :353-361; `(request, invalid)` passed at about 10 sites.
   - **Problem:**
     - The function is about 165 lines with 21 `return Read::outcome(started, …)` exits.
     - The "deadline, invalid, same window" check is written 3 times.
     - `(request, invalid)` is a missing type.
     - `scoped: bool` branches in 6 places.
     - The privacy classification (C2/D-4) is buried among the exits.
   - **Fix:**
     - Add `ReadGuard { request, window, invalid }` with `live() -> Result<(), ReadUnavailable>`.
     - Move the body into `read_checked() -> Result<Read, ReadUnavailable>` so that every exit is a `?`, and convert to `Read::outcome` once.
     - Extract `classify(password, Writability, focused)` and `fetch_strings`.
     - Keep the checkpoint positions where they are.
   - **Size:** M · **Behaviour:** none

5. **H-5** · medium · DRY
   - **Where:** `events.rs:182-259`, `hooks.rs:80-104,136-147`, `raw_loop.rs:68-99,115-126`, and also `core/src/platform/windows/clipboard.rs:176`.
   - **Problem:** The owner-thread handshake is written three times: named thread → `sync_channel(1)` carrying `Ok(thread_id)` or `Err` → `recv_timeout(2s)` → on failure, join only when governed/scoped → on stop, WM_QUIT and join. The "join only when governed" rule is undocumented and restated each time. The 2 s is a magic number.
   - **Fix:**
     - Add `mod owner` with `STARTUP_WAIT`, `OwnerThread<R> { thread_id, handle, join_on_failure }`, `spawn<T, R, E>(name, join_on_failure, body)` and `post_quit_and_join()`.
     - Document the join rule there.
   - **Size:** M · **Behaviour:** none

6. **H-6** · low · Idiomatic
   - **Where:** `input.rs:76-83` (`CaptureScope`), `elevated/src/runtime/input.rs:44-52`.
   - **Problem:** `CaptureScope` has three `Arc<dyn Fn>` fields, all filled from clones of one `Arc<Grant>`. Callers write `(scope.serial)()`.
   - **Fix:** Add `trait InputScope { hook_open; raw_open; serial }` and implement it for `Grant`.
   - **Size:** S · **Behaviour:** none

7. **H-7** · low · Debt / consistency
   - **Where:** `identity.rs:1-11,25-30,58-84`; `hung.rs:6-9,62`; `windows.rs:91-93`; `uia.rs:610-613`; core `context/hung.rs:112,116`.
   - **Problem:**
     - Plain Win32 calls mix the `windows` and `windows-sys` crates, so there are two `HWND` types and two `HANDLE` types. This forces wrapper types and `.0` accesses.
     - Handle ownership comes in three styles: `ProcessHandle` with a manual Drop, a local `File` RAII type, and a manual `CloseHandle` (`device.rs:139`).
     - Only the COM code (`uia.rs`, `uia_commits.rs`) actually needs `windows`.
   - **Fix:** Use `windows-sys` for all non-COM code, use `OwnedHandle` everywhere, and convert between the two at the UIA boundary only.
   - **Size:** M · **Behaviour:** none

8. **H-8** · low · DRY
   - **Where:** `held.rs:58-65`; `hung.rs:130-134,100`; `identity.rs:25-39`; `raw.rs:404-433`; `hooks.rs:280-296`; `ime.rs:55-61`.
   - **Problem:** "HWND → (thread, pid)" plus its re-check is written 6 times, with 4 different result shapes. `hung.rs` compares a `(pid, thread)` tuple.
   - **Fix:** Add `WindowOwner { thread: NonZeroU32, pid: NonZeroU32 }` and `window_owner(hwnd)` in `focus.rs`.
   - **Size:** S · **Behaviour:** none

9. **H-9** · low · DRY (privacy rule)
   - **Where:** `raw.rs:442-459`, `windows.rs:44-63`.
   - **Problem:** Both places build `FocusAtReceipt` from `FocusAt` the same way, including the privacy rule that the active-window fallback carries no `password_style` (INP-2, C4). The two copies could drift apart.
   - **Fix:** Add `FocusAt::receipt()` in `focus.rs`.
   - **Size:** S · **Behaviour:** none

10. **H-10** · low · Architecture / idiomatic
    - **Where:** `hid.rs:141,178,237,245,129,179-230`; `device.rs:253`; `raw.rs:63,370-374`.
    - **Problem:**
      - The collection kind is re-derived from the caps 4 times.
      - `ndof: Option` only has meaning for multi-axis devices.
      - The loops over value caps and button caps are almost the same.
      - `identify` returns `Option<Option<(DeviceId, DeviceId)>>`.
      - `sequence: [u64; 2]` is indexed by a bool.
    - **Fix:** Add `enum Layout { Ndof(..), Fields{..} }`, `enum Identified { .. }` and `struct Sequences { raw_input, optional_hid }`.
    - **Size:** M · **Behaviour:** none

11. **H-11** · low · Idiomatic (overlaps D8)
    - **Where:**
      - `lib.rs:72`, plus consumers at `events.rs:140`, `clipboard.rs:184`, `context.rs:401,412,448,1628`, `uia_commits.rs:147`
      - `events.rs:54-60,333-335`
      - `loss.rs:55-67` vs `:149-158`
      - `loss.rs:20-24,136-139`
    - **Problem:**
      - `(count, first, last)` is destructured at 6 sites, and `CommitLoss` repeats the same fields.
      - The `events.rs` ring re-implements the same bounds.
      - `Losses::lose` and `loss::producer` match the same variants separately.
      - A `Vec` is used as a FIFO with `remove(0)`.
    - **Fix:**
      - Add `LossSpan { count, first, last }`, returned by `LossCounter::take`, with a non-atomic `note()`.
      - Add one `receipt(record)`.
      - Use `VecDeque`.
    - **Size:** M · **Behaviour:** none

12. **H-12** · low · DRY / architecture
    - **Where:** `held.rs:67-95,110-139`; `elevated/src/process.rs:279-291`; `elevated/src/runtime/witness.rs:449-481`.
    - **Problem:**
      - This is a new argument for round-1 H9. The two integrity-RID parsers are more than duplicates; they validate differently. `held.rs` accepts any count > 0 and reads the last subauthority. `process.rs` requires count == 1 and `IsValidSid`, and reads index 0.
      - `on_input_desktop` sits in the held-keys module but is used by `preflight.rs` and `visit.rs`. `witness.rs` contains a third desktop-name reader.
    - **Fix:** Add a pure `integrity_rid(label, used) -> Option<u32>` that applies both validations, and a `desktop.rs` module for the desktop check.
    - **Size:** S · **Behaviour:** none, or a `change` if the stricter validation would reject a real token that `held.rs` accepts today

13. **H-13** · low · DRY
    - **Where:** `input.rs:148-150`; `hooks.rs:88-92`; `raw_loop.rs:82-86,222-226`; pointer packing at `hooks.rs:268` and `:434`.
    - **Problem:** The `NativeError { stage, code: error.raw_os_error(), .. }` literal is written 4 times. The packing of signed points is ad hoc.
    - **Fix:** Add `NativeError::io(stage, &io::Error)`, and do the point packing inside `Entry` (H-3).
    - **Size:** S · **Behaviour:** none

14. **H-14** · low · Idiomatic / debt
    - **Where:** `uia_commits.rs:92-95,152`; `hid.rs:202`; `input.rs:236`; `uia.rs:161-167,372-383`; `ime.rs:27-35`; `events.rs:107-113`.
    - **Problem:**
      - `lost[reason as usize]` depends on the enum's declaration order.
      - `hid.rs:202` sign-extends a nibble by hand while a `signed()` helper exists.
      - A REG_DWORD is assembled by hand from two `u16`s.
      - Elevated vs recorder mode has four spellings (`scoped` pairs in 4 modules, `governed` in `input.rs`).
    - **Fix:**
      - Add `CommitLossReason::ALL` and `index()` with a const assert.
      - Use `signed(..)` and `u32::from_le_bytes`.
      - Use one `Mode` enum or one name, and drop the single-caller wrapper pairs.
    - **Size:** S · **Behaviour:** none

## Assessment
The area is in good shape. The unsafe code is well commented, the bounds are explicit, the round-1 fixes hold, and there are no TODOs and no dev-log clutter.

Top debts:
1. The god functions `raw_loop` (H-1) and `read_inner` (H-4).
2. Duplicated delivery and thread-handshake code (H-2, H-5).
3. The untyped hook-ring encoding (H-3).

Fine as they are: `focus.rs`, `ndof.rs`, `LossCounter`, the `uia_commits` slot reservation, `MetadataScrubber`, the input helper binary, and `NativePod`/`native_copy`.
