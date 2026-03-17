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

## Flat struct vs embedded struct for JSON serialization

- When a struct mirrors fields from two source structs (e.g. `MemoryContext` copying from `MemInfo` + `VmStat`), every new field requires 3 edits: source struct, target struct, and builder function.
- Embedding source structs directly (`MemoryContext { meminfo: MemInfo, vmstat: VmStat }`) eliminates duplication. New parser fields flow through automatically.
- Trade-off: changes the JSON output format from flat to nested. Acceptable for internal tools pre-1.0.

## macOS vs Linux procfs/sysfs compatibility in tests

- Smoke tests running on macOS CI will get `null` for `/proc/meminfo`, `/proc/pressure/`, etc.
- Use `json.get("field").is_some_and(|v| !v.is_null())` instead of `if let Some(v) = json.get("field")` — `serde_json` serializes `None` as JSON `null`, so `.get()` returns `Some(Value::Null)`, not `None`.

## Clippy pedantic patterns (edition 2024)

- `find(|(_, &c)| c > 0)` → use `find(|(_, c)| **c > 0)` — explicit dereference in pattern is not allowed in implicitly-borrowing iterator context.
- Nested `if let Some(..) { if cond { ... } }` → collapse with `.filter()`: `if let Some(v) = expr.filter(pred) { ... }`.
