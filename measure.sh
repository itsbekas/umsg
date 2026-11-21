#!/usr/bin/env bash
set -euo pipefail

BIN="${1:?Usage: ./measure.sh <path-to-binary>}"

echo "=== Binary size ==="
ls -lh "$BIN"

# Start server in background
"$BIN" >/dev/null 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true' EXIT

# Wait briefly for it to start listening
sleep 0.2

echo ""
echo "=== Idle memory (before any request) ==="
grep -E '^(Rss|Pss|Private_Dirty|Private_Clean|Shared_Clean):' "/proc/$PID/smaps_rollup"

# Send one real HTTP request
curl -s http://127.0.0.1:8080/ >/dev/null

echo ""
echo "=== Memory after 1 request ==="
grep -E '^(Rss|Pss|Private_Dirty|Private_Clean|Shared_Clean):' "/proc/$PID/smaps_rollup"

echo ""
echo "=== Where is the RAM coming from? (regions with Rss > 0) ==="
awk '
/^[0-9a-f]+-/ {
    name = ($6 != "" ? $6 : "[anonymous]")
    next
}
/^Rss:/ {
    if ($2 > 0) rss[name] += $2
}
END {
    for (n in rss) printf "%8d kB  %s\n", rss[n], n
}
' "/proc/$PID/smaps" | sort -rn
