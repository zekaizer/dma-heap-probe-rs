#!/bin/bash
# Auto-format Rust files after Write/Edit tool use.
# Reads PostToolUse JSON from stdin; runs cargo fmt only for .rs files.

FILE_PATH=$(jq -r '.tool_input.file_path // empty' 2>/dev/null)

if [[ -z "$FILE_PATH" ]]; then
  exit 0
fi

if [[ "$FILE_PATH" == *.rs ]]; then
  cargo fmt 2>/dev/null
fi

exit 0
