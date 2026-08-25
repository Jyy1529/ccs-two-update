#!/usr/bin/env bash
# Compatibility entry point retained from v3.19.3. The old script targeted
# non-existent HTTP endpoints and confused pi.dev with an Inflection model.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

echo "== DeepSeek/Pi frontend regressions =="
pnpm test:unit \
  tests/components/AboutSection.deepseekPi.test.tsx \
  tests/components/ProviderForm.deepseekPi.test.tsx \
  tests/components/McpFormModal.test.tsx \
  tests/hooks/useDirectorySettings.test.tsx

echo "== DeepSeek official config projection =="
cargo test --manifest-path src-tauri/Cargo.toml deepseek_config --lib

echo "== Pi official config projection =="
cargo test --manifest-path src-tauri/Cargo.toml pi_config --lib

echo "== CLI lifecycle metadata =="
cargo test --manifest-path src-tauri/Cargo.toml \
  deepseek_and_pi_lifecycle_metadata_is_consistent --lib

echo "== Unsupported MCP targets =="
cargo test --manifest-path src-tauri/Cargo.toml \
  mcp_apps_ignore_legacy_deepseek_and_pi_flags --lib
cargo test --manifest-path src-tauri/Cargo.toml \
  upsert_mcp_server_ignores_deepseek_and_pi_mcp_flags --test mcp_commands

echo "DeepSeek/Pi focused regressions passed."
