# Lessons Learned

Patterns and corrections discovered during development. Updated after each mistake or user correction.

---

## Rust macro format args limitation

- `prop_assert!`, `prop_assert_ne!`, and similar macro wrappers around `format_args!` do NOT support inline variable capture (`{var:?}` syntax). Must use positional args: `"msg: {:?}", var`.
- Same applies to any macro that expands to `format_args!` internally.
- Always use explicit positional arguments in assertion macros from external crates.

## Test result verification

- Never report test results without actually reading the full output. If a tool execution was rejected or interrupted, re-run the test before claiming it passed.
- Always verify both compilation AND runtime results — a test that compiles doesn't mean it passes.
