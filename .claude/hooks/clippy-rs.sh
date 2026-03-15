#!/bin/bash
# Run cargo clippy after Write/Edit on .rs files.
# Exit 2 = block with feedback to Claude; exit 0 = continue.

INPUT=$(cat)
FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty' 2>/dev/null)

if [[ -z "$FILE_PATH" ]] || [[ "$FILE_PATH" != *.rs ]]; then
  exit 0
fi

OUTPUT=$(cargo clippy --message-format=short 2>&1)
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]] || echo "$OUTPUT" | grep -q "^warning\|^error"; then
  echo "$OUTPUT" >&2
  exit 2
fi

exit 0
