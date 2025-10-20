#!/usr/bin/env bash
# Performance profiling script for sy

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROFILE_DIR="$PROJECT_ROOT/target/profile"

cd "$PROJECT_ROOT"

echo "🔍 Performance Profiling for sy"
echo "================================"
echo ""

# Create profile directory
mkdir -p "$PROFILE_DIR"

# Build release binary
echo "📦 Building release binary..."
cargo build --release --quiet

echo ""
echo "🔥 Generating flamegraphs for different workloads..."
echo ""

# Create test data directories
TEST_SRC="$PROFILE_DIR/src"
TEST_DST="$PROFILE_DIR/dst"
rm -rf "$TEST_SRC" "$TEST_DST"
mkdir -p "$TEST_SRC" "$TEST_DST"

# Scenario 1: Many small files (typical project directory)
echo "1️⃣  Profiling: 1000 small files (1-10KB each)"
for i in {1..1000}; do
    dd if=/dev/urandom of="$TEST_SRC/file_$i.txt" bs=1024 count=$((RANDOM % 10 + 1)) 2>/dev/null
done

sudo cargo flamegraph --output "$PROFILE_DIR/flamegraph_small_files.svg" \
    --bin sy -- "$TEST_SRC" "$TEST_DST" --quiet 2>/dev/null || echo "  ⚠️  Flamegraph failed (may need sudo)"

# Clean for next test
rm -rf "$TEST_DST"
mkdir -p "$TEST_DST"

# Scenario 2: Large file (delta sync candidate)
echo "2️⃣  Profiling: 1 large file (100MB)"
dd if=/dev/urandom of="$TEST_SRC/large.bin" bs=1048576 count=100 2>/dev/null

sudo cargo flamegraph --output "$PROFILE_DIR/flamegraph_large_file.svg" \
    --bin sy -- "$TEST_SRC/large.bin" "$TEST_DST/large.bin" --quiet 2>/dev/null || echo "  ⚠️  Flamegraph failed (may need sudo)"

# Scenario 3: Delta sync (modify file and re-sync)
echo "3️⃣  Profiling: Delta sync (1MB change in 100MB file)"
if [ -f "$TEST_DST/large.bin" ]; then
    cp "$TEST_DST/large.bin" "$TEST_DST/large_backup.bin"
    # Modify 1MB in the middle of the file
    dd if=/dev/urandom of="$TEST_SRC/large.bin" bs=1048576 count=1 seek=50 conv=notrunc 2>/dev/null

    sudo cargo flamegraph --output "$PROFILE_DIR/flamegraph_delta.svg" \
        --bin sy -- "$TEST_SRC/large.bin" "$TEST_DST/large.bin" --quiet 2>/dev/null || echo "  ⚠️  Flamegraph failed (may need sudo)"
else
    echo "  ⚠️  Skipping delta sync flamegraph (dst file missing)"
fi

echo ""
echo "✅ Flamegraphs generated in: $PROFILE_DIR"
echo ""
echo "📊 Running benchmarks vs rsync..."
echo ""

# Clean test data
rm -rf "$TEST_SRC" "$TEST_DST"
mkdir -p "$TEST_SRC" "$TEST_DST"

# Benchmark: Many small files
echo "Benchmark 1: 1000 small files (1-10KB)"
for i in {1..1000}; do
    dd if=/dev/urandom of="$TEST_SRC/file_$i.txt" bs=1024 count=$((RANDOM % 10 + 1)) 2>/dev/null
done

echo -n "  sy:    "
/usr/bin/time -p "$PROJECT_ROOT/target/release/sy" "$TEST_SRC" "$TEST_DST" --quiet 2>&1 | grep real

rm -rf "$TEST_DST"
mkdir -p "$TEST_DST"

echo -n "  rsync: "
/usr/bin/time -p rsync -a "$TEST_SRC/" "$TEST_DST/" 2>&1 | grep real

speedup=$(echo "scale=2; $(rsync -a "$TEST_SRC/" "$TEST_DST/" 2>&1 | grep -o '[0-9.]*s' | head -1 | tr -d 's') / $(\"$PROJECT_ROOT/target/release/sy\" "$TEST_SRC" "$TEST_DST" --quiet 2>&1 | grep -o '[0-9.]*s' | head -1 | tr -d 's')" | bc 2>/dev/null || echo "N/A")
echo "  Speedup: ${speedup}x"

echo ""

# Benchmark: Large file
echo "Benchmark 2: 100MB file"
rm -rf "$TEST_SRC" "$TEST_DST"
mkdir -p "$TEST_SRC" "$TEST_DST"
dd if=/dev/urandom of="$TEST_SRC/large.bin" bs=1048576 count=100 2>/dev/null

echo -n "  sy:    "
/usr/bin/time -p "$PROJECT_ROOT/target/release/sy" "$TEST_SRC/large.bin" "$TEST_DST/large.bin" --quiet 2>&1 | grep real

rm "$TEST_DST/large.bin"

echo -n "  rsync: "
/usr/bin/time -p rsync -a "$TEST_SRC/large.bin" "$TEST_DST/large.bin" 2>&1 | grep real

echo ""

# Benchmark: Delta sync
echo "Benchmark 3: Delta sync (1MB change in 100MB)"
cp "$TEST_SRC/large.bin" "$TEST_DST/large.bin"
dd if=/dev/urandom of="$TEST_SRC/large.bin" bs=1048576 count=1 seek=50 conv=notrunc 2>/dev/null

echo -n "  sy:    "
/usr/bin/time -p "$PROJECT_ROOT/target/release/sy" "$TEST_SRC/large.bin" "$TEST_DST/large.bin" --quiet 2>&1 | grep real

cp "$TEST_SRC/large.bin" "$TEST_DST/large_backup.bin"
dd if=/dev/urandom of="$TEST_SRC/large.bin" bs=1048576 count=1 seek=50 conv=notrunc 2>/dev/null

echo -n "  rsync: "
/usr/bin/time -p rsync -a "$TEST_SRC/large.bin" "$TEST_DST/large.bin" 2>&1 | grep real

echo ""
echo "✅ Profiling complete!"
echo ""
echo "📁 Results:"
echo "  - Flamegraphs: $PROFILE_DIR/*.svg"
echo "  - Open with: open $PROFILE_DIR/flamegraph_*.svg"
