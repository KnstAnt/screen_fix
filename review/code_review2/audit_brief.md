# Architecture and technical-debt audit brief (screen_fix, round 2)

Repo: `S:\myProgr\proj\rust\screen_fix`. It is a Rust workspace for a Windows screen recorder, about 51k lines. The code compiles cleanly and was merged to master as the new baseline.

A previous round (`review/code_review/findings_*.md`) has already been applied. Do not re-report what it fixed. Re-report a point it skipped only if you have a new argument.

This round is about **architecture and technical debt**, not bug hunting. The user wants professional, idiomatic Rust that is simple to read and change.

## Context to skim first (do not review the docs themselves)
- `doc/SCREEN_FIX_M1_ARCHITECTURE_ADDENDUM.md`: ownership, threads, and the process-wide statics that are permitted.
- `review/DEVELOPER_HANDOFF.md`: the vocabulary (loss ledger, fault classes F/L/G, and so on).
- `doc/SCREEN_FIX_M1_JOURNAL_SCHEMA.md`: the serialized contracts. Serialized names and values are fixed.

## What to look for, in priority order
1. **Architecture and responsibilities (SOLID, layering).**
   - God objects and god functions: structs with many unrelated fields, functions that mix several responsibilities, modules over ~1500 lines that hold more than one concept.
   - Wrong layering: platform code holding domain rules that belong in `core/src/capture` or `package`, domain types that depend on Windows details, `protocol` holding logic.
   - Hidden coupling: a component that knows another's internals, long parameter lists threading state through many layers, the same tuple of values passed together everywhere (a missing type).
   - Missing or leaky abstractions: an enum or trait that would remove repeated `match`/`if` chains; a state machine encoded in booleans or `Option`s.
2. **Duplication (DRY).**
   - The same logic in two or more places, within your area or across the workspace. Grep the whole workspace.
   - Near-duplicates that differ only in a constant or a type, and should be one generic or parameterized helper.
3. **KISS.**
   - Over-engineering: indirection, generics, traits or channels that buy nothing.
   - Convoluted control flow: deep nesting, flags, `loop` + `continue` mazes, where a simpler structure says the same.
   - Clever code that a reader must simulate to understand.
4. **Idiomatic Rust.**
   - Stringly-typed values.
   - Manual reimplementations of std or iterator combinators.
   - `as` casts that can truncate.
   - Needless `clone`, allocation or `Arc`/`Mutex` where ownership would do.
   - `unwrap`/`expect` on fallible paths.
   - Error types that lose context.
   - Visibility that is wider than needed.
   - Dead code and stale comments.
5. **Technical debt markers.**
   - TODO/FIXME/HACK comments.
   - Temporary workarounds and dev-only code left on production paths. The `dev-log` feature is known; flag only call sites that clutter the logic.
   - Inconsistent conventions between modules that do the same kind of work.

## Rules
- **Read-only for code.** Do not edit any source file. Do not run cargo, clippy, tests, git or the app.
- **Write your findings to the file named in your prompt**, in the output format below. That file is the only one you may create or write.
- **Verify every finding in the code.** Open the callers and the callees. If you are unsure, mark the finding PLAUSIBLE and say what would confirm it.
- **Ignore pure formatting and naming taste.**
- **Prefer impact over count.** A refactor is worth reporting only if it clearly makes the code simpler, safer or easier to change. Estimate its size. There is no quota; an honest "this area is clean" is fine.
- **Never propose weakening a privacy, approval, security or trust check, or changing a serialized contract.** A behaviour change is allowed only if it is flagged as `change`.

## Output format (the findings file)
A numbered list. Each finding gives:
- **id**: your area letter plus a number;
- **severity**: high / medium / low, by the maintainability or correctness cost;
- **category**: one of the 5 above;
- **file:line**: one or more locations;
- **problem**: 1–3 sentences, with the concrete cost;
- **fix**: minimal and concrete; name the new type, helper or split;
- **size**: S (< 30 lines) / M (< 200) / L (larger);
- **behaviour**: `none` (refactor only), `bugfix` (name the violated contract), or `change` (needs the user's approval).

End the file with a 3–5 line assessment of your area: overall health, the top 3 debts, and what is fine as is.

Your final message to the coordinator: at most 150 words. Give the counts by severity, the top 3 findings in one line each, and the findings file path.
