# Round-2 execution brief (phases 1–3)

Repo: `S:\myProgr\proj\rust\screen_fix` (Rust, Windows recorder). Baseline: `74ed5af`.

Read `review/code_review2/AUDIT_SUMMARY.md` first. The detail for each finding id (A1…X18) is in `review/code_review2/findings_<area>.md`.

## User decisions (2026-11-18)
- **D1. dev-log guard:** already done. `compile_error!` fires on `elevated-release` + `dev-log` (ui/src/main.rs).
- **D2. Tests:** minimal tests are allowed:
  - golden tests of journal-row bytes;
  - one package integration test (`validate_gates` on a synthetic attempt, E6).

  Write them, and keep them compiling (`cargo check --all-targets`). Do NOT run them; the user runs tests.
- **D3. Windows-only:** the workspace is Windows-only (X14, change). Remove the `cfg(not(windows))` stubs and the non-Windows dependencies, and make the platform API unconditional.
- **D4. Behaviour changes:**
  - G6: only the `none` variant. Share `tick_delta_ms` and the uncertainty constant; keep the collector's bound.
  - H12 and C15: do not touch.
- **D5. Scope:** phases 1–3 now. Phase 4 (the god-object splits) and phase 5 (polling to events) come later.

## Phases
- **Phase 1 (T4): shared primitives.** Introduce each helper once and replace every copy across the workspace. Run in three sequential parts: 1a, 1b, 1c.
- **Phase 2 (T2): typed journal rows** (X5, B2, B3, C10, C11 envelope part, D3, E2, E3). Write the golden tests before changing any row builder.
- **Phase 3 (T3): layers** (X2, X3, C4, E1, X4/B4 finalize, F15, F16, X17, X14).

## Rules
- **Refactor only.** Behaviour stays the same unless an item above says `change`.
  - Keep every serialized name and value byte-identical.
  - Do not change any wire layout or `attachment::VERSION`.
  - Never weaken a privacy, approval, security or trust check. Where copies of a check are merged, the merged version must be exactly as strict as the strictest copy. If the copies differ, keep them separate and report it.
- **Rounding.** When merging numeric helpers (QPC↔time), keep each call site's existing rounding and overflow behaviour. Use separate helper variants if needed.
- **Code style.** Idiomatic and minimal:
  - no dead code;
  - minimal visibility;
  - named constants;
  - correct doc comments and SAFETY comments;
  - no `unwrap`/`expect` on fallible paths.
- **Files.** LF line endings and valid UTF-8 (a cp1252 `§` slipped in once before; check it).
- **Commands.**
  - Run `cargo check --workspace --all-targets` at the end, and fix until 0 errors and 0 warnings.
  - Also run `cargo check -p screen-fix-native-observation --features context` and `cargo check -p screen-fix --features fixture-save`.
  - Do not run tests, clippy, fmt or the app.
  - Git: read-only.
- **Reporting.** The harness forbids subagents from writing report files. Put the report in your final message.

## Report (final message)
- Item ids: done / partly / skipped, with the reason.
- Files touched.
- Cargo results.
- Docs that need an update.
- Anything the coordinator must decide.
- Keep it under ~500 words.
