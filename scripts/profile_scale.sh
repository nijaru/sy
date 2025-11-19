#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BENCH_DIR="$PROJECT_ROOT/target/scale_bench_data"
RESULTS_DIR="$PROJECT_ROOT/target/profiling"

# Ensure we're in project root
cd "$PROJECT_ROOT"

echo "🚀 Scale Profiling Setup"
echo "========================"

# 1. Build binaries
echo "📦 Building binaries..."
cargo build --release --bin sy --bin sy-bench-gen --quiet

# 2. Generate Dataset
if [ -d "$BENCH_DIR" ]; then
    echo "✅ Dataset exists at $BENCH_DIR"
else
    echo "Generating 100,000 files (this may take a moment)..."
    "$PROJECT_ROOT/target/release/sy-bench-gen" \
        --root "$BENCH_DIR/src" \
        --count 100000 \
        --depth 5 \
        --width 50 \
        --min-size 100 \
        --max-size 1000
fi

mkdir -p "$RESULTS_DIR"
mkdir -p "$BENCH_DIR/dst"

# 3. Profile Scan Phase (Dry Run)
echo ""
echo "📊 Profiling Scan Phase (100k files, dry-run)"
echo "---------------------------------------------"

SY_BIN="$PROJECT_ROOT/target/release/sy"

# Platform-specific time command
if [[ "$OSTYPE" == "darwin"* ]]; then
    TIME_CMD="/usr/bin/time -l"
else
    TIME_CMD="/usr/bin/time -v"
fi

echo "Running memory profile..."
$TIME_CMD "$SY_BIN" "$BENCH_DIR/src" "$BENCH_DIR/dst" --dry-run --stream --json > "$RESULTS_DIR/scan_output.json" 2> "$RESULTS_DIR/scan_profile.txt"

if command -v hyperfine &> /dev/null; then
    echo "Running speed benchmark..."
    hyperfine --warmup 1 --runs 3 \
        --export-markdown "$RESULTS_DIR/benchmark.md" \
        "$SY_BIN \"$BENCH_DIR/src\" \"$BENCH_DIR/dst\" --dry-run --stream"
fi

echo ""
echo "📈 Results:"
if [[ "$OSTYPE" == "darwin"* ]]; then
    grep "maximum resident set size" "$RESULTS_DIR/scan_profile.txt"
else
    grep "Maximum resident set size" "$RESULTS_DIR/scan_profile.txt"
fi
grep "real" "$RESULTS_DIR/scan_profile.txt" || true

echo ""
echo "See full profile at: $RESULTS_DIR/scan_profile.txt"
