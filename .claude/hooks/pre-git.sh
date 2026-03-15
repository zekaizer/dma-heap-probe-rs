#!/bin/bash
# PreToolUse hook: run cargo test + clippy before git push or git commit.

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)

if ! echo "$COMMAND" | grep -qE "git (push|commit)"; then
  exit 0
fi

echo "Running cargo test..." >&2
TEST_OUTPUT=$(RUSTFLAGS="-Awarnings" cargo test 2>&1)
if [[ $? -ne 0 ]]; then
  echo "cargo test failed:" >&2
  echo "$TEST_OUTPUT" >&2
  exit 2
fi

echo "Running cargo clippy..." >&2
CLIPPY_OUTPUT=$(cargo clippy --message-format=short 2>&1)
if [[ $? -ne 0 ]] || echo "$CLIPPY_OUTPUT" | grep -q "^warning\|^error"; then
  echo "cargo clippy failed:" >&2
  echo "$CLIPPY_OUTPUT" >&2
  exit 2
fi

echo "All checks passed." >&2
exit 0
