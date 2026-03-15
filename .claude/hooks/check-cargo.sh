#!/bin/bash
# PostToolUse hook: run cargo clippy after Cargo.toml is modified.

INPUT=$(cat)
FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty' 2>/dev/null)

if [[ "$FILE_PATH" != */Cargo.toml ]]; then
  exit 0
fi

OUTPUT=$(cargo clippy --message-format=short 2>&1)
if [[ $? -ne 0 ]] || echo "$OUTPUT" | grep -q "^warning\|^error"; then
  echo "cargo clippy failed after Cargo.toml change:" >&2
  echo "$OUTPUT" >&2
  exit 2
fi

exit 0
