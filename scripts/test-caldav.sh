#!/usr/bin/env bash
# Starts a throwaway Radicale CalDAV server on http://localhost:5232 (any login is accepted),
# storing into target/test-caldav. Needs uv (https://docs.astral.sh/uv/).
# Usage: scripts/test-caldav.sh [stop]
set -euo pipefail
dir="$(cd "$(dirname "$0")/.." && pwd)/target/test-caldav"
mkdir -p "$dir"
if [[ -f "$dir/pid" ]]; then kill "$(cat "$dir/pid")" 2>/dev/null || true; rm -f "$dir/pid"; fi
if [[ "${1:-}" == stop ]]; then exit; fi
rm -rf "$dir/collections"
uvx --quiet radicale@3.8.3 --server-hosts localhost:5232 --auth-type none \
  --storage-filesystem-folder "$dir/collections" --logging-level warning >"$dir/log" 2>&1 &
echo $! >"$dir/pid"
for _ in $(seq 50); do curl -s -o /dev/null http://localhost:5232/ && break; sleep 0.2; done
echo "Radicale running on http://localhost:5232 (log: $dir/log)"
