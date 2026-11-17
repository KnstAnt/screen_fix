# Code-quality review brief (screen_fix, Rust workspace for a Windows screen recorder)

Repo: `S:\myProgr\proj\rust\screen_fix`. The code compiles cleanly (`cargo check --workspace --all-targets`: 0 errors, 0 warnings). The user wants professional, idiomatic Rust that meets the highest standards.

## Context to skim first (do not review the docs themselves)
- `doc\SCREEN_FIX_M1_ARCHITECTURE_ADDENDUM.md`: ownership, threads, the permitted process-wide statics.
- `review\DEVELOPER_HANDOFF.md`: the vocabulary of the recent remediation (loss ledger, fault classes F/L/G, etc.).
- `doc\SCREEN_FIX_M1_JOURNAL_SCHEMA.md`: serialized contracts. Serialized names and values are fixed contracts.

## What to look for, in priority order
1. **Contract violations.**
   - The code breaks a documented invariant: doc comments, module docs, the plan or spec references in comments, or the journal schema.
   - Types or functions whose behaviour contradicts their name or doc.
   - Callers that rely on something the callee does not guarantee.
2. **Recovery where recovery is impossible.**
   - Retry loops or fallbacks on errors that cannot heal: a poisoned state, a closed channel, a lost device without rebuild, an invariant breach.
   - Errors swallowed with `let _ =`, `.ok()`, `unwrap_or_default` or `filter_map(Result::ok)` where the failure matters.
   - "Best effort" paths that hide a fault the plan says must be F/L.
   - Conversely, panics (`unwrap`, `expect`, indexing, arithmetic overflow) on paths where an error is possible.
3. **Encapsulation violations.**
   - Fields or functions more visible than needed (`pub` vs `pub(crate)`/`pub(super)`).
   - Modules reaching into another component's internals or mutating state they do not own.
   - Shared mutable state outside the documented exceptions.
   - Leaky abstractions.
4. **Duplication.**
   - The same logic in two or more places, including across files in your area.
   - A local re-implementation of a helper that already exists elsewhere in the workspace; grep for it.
5. **Redundant checks.**
   - Conditions that the types or earlier code already guarantee.
   - Double validation.
   - Defensive checks for impossible states that add noise. A defensive check that guards a real cross-thread or OS boundary is fine.
6. **Other bad practice and non-idiomatic code.**
   - Stringly-typed values where an enum exists.
   - Magic numbers.
   - Overly long functions with mixed responsibilities.
   - Needless clones or allocations on hot paths.
   - `unsafe` without a clear safety argument, or with a wrong one.
   - Lock ordering hazards.
   - Busy-wait or sleep polling where a blocking primitive fits.
   - `as` casts that can truncate silently.
   - Error types that lose context.
   - Comments that are wrong or out of date.
   - Dead code.

## Rules
- READ-ONLY. Do not edit files. Do not run cargo, clippy, tests, git or the app.
- Verify every finding in the code: open the callers and callees. No speculation. If unsure, mark the finding PLAUSIBLE and say what would confirm it.
- Ignore pure formatting and naming taste.
- Prefer findings with real impact over many cosmetic ones. There is no quota; an honest "this area is clean" is fine.

## Output (final message, English)
A numbered list. Each finding gives:
- **id** (your area letter + number), **severity** (high / medium / low), **category** (one of the 6 above), **file:line**;
- **problem:** 1–3 sentences, with the concrete failure or cost;
- **fix:** minimal and concrete;
- **behaviour:** `none` (refactor only), `bugfix` (the current behaviour is wrong under a contract), or `change` (a design or behaviour change that the user should approve). For a bugfix, name the contract.

Finish with a 2–3 line overall assessment of your area.
