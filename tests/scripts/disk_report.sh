#!/usr/bin/env bash
# What this checkout is using, and why.
#
# Build artifacts grow in a way nobody notices until a write fails, because
# the growth is not waste — it is one complete set of artifacts per feature
# combination, and wasmtime is several gigabytes of them. That is invisible
# until you go looking, so this makes it a command rather than a habit.

set -uo pipefail
cd "$(dirname "$0")/../.."

echo "== disk =="
df -h . | tail -1
echo

echo "== build artifacts =="
total=0
for dir in target tests/fixtures/*/target; do
  [ -d "$dir" ] || continue
  size=$(du -sk "$dir" 2>/dev/null | cut -f1)
  total=$((total + size))
  printf '  %-45s %s\n' "$dir" "$(du -sh "$dir" 2>/dev/null | cut -f1)"
done
printf '  %-45s %s\n' "(total)" "$((total / 1024))M"
echo

echo "== feature combinations that each keep their own set =="
cat <<'TXT'
  cargo build --workspace                     default, with the plugin compiler
  cargo build -p daemon --no-default-features no plugin subsystem at all
  -p daemon --features plugins                plugins without the compiler
TXT
echo
cat <<'TXT'
Note: tests/fixtures/*/target belong to separate workspaces, so the top-level
`cargo clean` does not touch them. To reclaim everything:

  cargo clean
  find tests/fixtures -name target -maxdepth 2 -type d -exec rm -rf {} +

The fixture components are rebuilt on demand by `cargo test -p wasm-host`.
TXT
