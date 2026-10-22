#!/usr/bin/env bash
# Siftmail: AI/embedding output may only produce suggestions. Assert that the
# modules producing model output never reference mailbox action types.
set -euo pipefail
cd "$(dirname "$0")/.."
src=crates/flectar-mail-core/src
paths=("$src/ai" "$src/embed" "$src/suggest" "$src/category/suggest_llm.rs")
pattern='ActionKind|ActionParams|PerformActionArgs|perform_action|AiAutomationAction|apply_thread_action|RuleAction'
if rg -n "$pattern" "${paths[@]}"; then
  echo "error: AI/embedding/suggest modules must not reference mailbox actions" >&2
  exit 1
fi
echo "suggest boundary ok"
