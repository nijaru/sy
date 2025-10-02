#!/usr/bin/env bash
# Compare benchmark performance between two commits
# Usage: ./scripts/bench-compare.sh <baseline-commit> [comparison-commit]

set -e

BASELINE=${1:-main}
COMPARISON=${2:-HEAD}

echo "=== Benchmark Comparison ==="
echo "Baseline:   $BASELINE"
echo "Comparison: $COMPARISON"
echo ""

# Create temp directory for results
TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

# Save current branch
CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
CURRENT_COMMIT=$(git rev-parse HEAD)

# Benchmark baseline
echo "Running baseline benchmarks ($BASELINE)..."
git checkout -q "$BASELINE"
cargo bench --bench sync_bench -- --save-baseline baseline 2>&1 | tee "$TEMP_DIR/baseline.txt"

# Benchmark comparison
echo ""
echo "Running comparison benchmarks ($COMPARISON)..."
git checkout -q "$COMPARISON"
cargo bench --bench sync_bench -- --baseline baseline 2>&1 | tee "$TEMP_DIR/comparison.txt"

# Restore original branch
git checkout -q "$CURRENT_BRANCH"
if [ "$CURRENT_COMMIT" != "$(git rev-parse HEAD)" ]; then
    git checkout -q "$CURRENT_COMMIT"
fi

echo ""
echo "=== Results ==="
echo ""
echo "Baseline results:"
grep -E "time:|thrpt:" "$TEMP_DIR/baseline.txt" | head -20 || true

echo ""
echo "Comparison results (shows % change):"
grep -E "time:|thrpt:|change:" "$TEMP_DIR/comparison.txt" | head -30 || true

echo ""
echo "Full results stored in:"
echo "  Baseline:   $TEMP_DIR/baseline.txt"
echo "  Comparison: $TEMP_DIR/comparison.txt"
echo ""
echo "To view detailed criterion reports:"
echo "  open target/criterion/report/index.html"
