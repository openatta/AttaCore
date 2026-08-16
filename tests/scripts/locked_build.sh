#!/usr/bin/env bash
# Verify the plugin-free build.
#
# Some deployments must be able to state that the binary they ship contains no
# plugin execution path at all. That claim is only true if the plugin crates
# are absent from the dependency graph, so this checks the graph rather than
# trusting the feature flag.
#
# `-p daemon --no-default-features` is not interchangeable with
# `--workspace --no-default-features`: cargo unifies features across the
# graph, so any other member that depends on daemon with `plugins` on turns it
# back on. The locked artifact must be built for this one package alone.

set -euo pipefail
cd "$(dirname "$0")/../.."

echo "==> building daemon without the plugins feature"
cargo build -p daemon --no-default-features

echo "==> asserting the plugin crates are absent from the dependency graph"
leaked=$(cargo tree -p daemon --no-default-features -e normal \
  | grep -Eo '\b(plugin|plugin-host|wasmtime) v' || true)
if [ -n "$leaked" ]; then
  echo "FAIL: plugin machinery is still linked into the locked build:" >&2
  echo "$leaked" >&2
  exit 1
fi

echo "==> running the daemon test suite in the locked configuration"
cargo test -p daemon --no-default-features

echo "==> the DSH bridge is a separate process and has its own suite"
if command -v node >/dev/null 2>&1; then
  (cd bridges/atta-dsh-bridge && node --test 'test/**/*.test.js' >/dev/null)
  echo "    bridge tests pass"
else
  echo "    skipped: node is not installed"
fi

echo "OK: locked build contains no plugin subsystem"
