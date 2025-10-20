#!/usr/bin/env bash
# Test the impact of std::mem::take optimization

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_ROOT"

echo "🧪 Testing delta sync optimization impact"
echo "=========================================="
echo ""

# Create test data (100MB file)
TEST_DIR="/tmp/sy_delta_test"
rm -rf "$TEST_DIR"
mkdir -p "$TEST_DIR"

echo "📝 Creating 100MB test file..."
dd if=/dev/urandom of="$TEST_DIR/test.bin" bs=1048576 count=100 2>/dev/null
cp "$TEST_DIR/test.bin" "$TEST_DIR/test_dst.bin"

echo "✏️  Modifying 1MB in the middle..."
dd if=/dev/urandom of="$TEST_DIR/test.bin" bs=1048576 count=1 seek=50 conv=notrunc 2>/dev/null

echo ""
echo "Running 5 iterations with current optimizations..."
echo ""

cargo build --release --quiet

times=()
for i in {1..5}; do
    # Reset destination
    cp "$TEST_DIR/test_dst.bin" "$TEST_DIR/test_reset.bin"

    # Modify source slightly different each time
    dd if=/dev/urandom of="$TEST_DIR/test.bin" bs=1048576 count=1 seek=$((50 + i)) conv=notrunc 2>/dev/null

    # Time the sync
    start=$(gdate +%s.%N)
    "$PROJECT_ROOT/target/release/sy" "$TEST_DIR/test.bin" "$TEST_DIR/test_reset.bin" --quiet
    end=$(gdate +%s.%N)
    elapsed=$(echo "$end - $start" | bc)
    times+=("$elapsed")
    printf "  Run %d: %.3fs\n" "$i" "$elapsed"
done

# Calculate median
IFS=$'\n' sorted=($(sort -n <<<"${times[*]}"))
median="${sorted[2]}"

echo ""
printf "Median time: %.3fs\n" "$median"

echo ""
echo "🧹 Cleaning up..."
rm -rf "$TEST_DIR"
