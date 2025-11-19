#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BENCH_DIR="$PROJECT_ROOT/target/scale_bench_data"
SY_SCAN="$PROJECT_ROOT/target/release/sy-scan"

# Platform-specific time command
if [[ "$OSTYPE" == "darwin"* ]]; then
    TIME_CMD="/usr/bin/time -l"
else
    TIME_CMD="/usr/bin/time -v"
fi

echo "📊 Profiling Just Scanner (100k files)"
echo "--------------------------------------"

$TIME_CMD "$SY_SCAN" "$BENCH_DIR/src"
