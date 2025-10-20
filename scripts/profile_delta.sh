#!/usr/bin/env bash
# Profile delta sync performance

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROFILE_DIR="$PROJECT_ROOT/target/profile_delta"

cd "$PROJECT_ROOT"

echo "🔍 Profiling delta sync performance"
echo "====================================="
echo ""

# Build release binary
echo "📦 Building release binary..."
cargo build --release --quiet

# Create test data
rm -rf "$PROFILE_DIR"
mkdir -p "$PROFILE_DIR/src" "$PROFILE_DIR/dst"

echo "📝 Creating 100MB test file..."
dd if=/dev/urandom of="$PROFILE_DIR/src/large.bin" bs=1048576 count=100 2>/dev/null

echo "📋 Initial sync..."
"$PROJECT_ROOT/target/release/sy" "$PROFILE_DIR/src/large.bin" "$PROFILE_DIR/dst/large.bin" --quiet

echo "✏️  Modifying 1MB in the middle..."
dd if=/dev/urandom of="$PROFILE_DIR/src/large.bin" bs=1048576 count=1 seek=50 conv=notrunc 2>/dev/null

echo ""
echo "🔥 Profiling delta sync with samply..."
echo ""

# Run with samply (will open in browser)
samply record --save-only --output "$PROFILE_DIR/delta_profile.json" -- \
    "$PROJECT_ROOT/target/release/sy" "$PROFILE_DIR/src/large.bin" "$PROFILE_DIR/dst/large.bin" --quiet

echo ""
echo "✅ Profile saved to: $PROFILE_DIR/delta_profile.json"
echo "   View with: samply load $PROFILE_DIR/delta_profile.json"
echo ""

# Also run a quick comparison
echo "📊 Quick comparison:"
echo ""

# sy delta sync
start=$(gdate +%s.%N)
"$PROJECT_ROOT/target/release/sy" "$PROFILE_DIR/src/large.bin" "$PROFILE_DIR/dst/large.bin" --quiet
end=$(gdate +%s.%N)
sy_time=$(echo "$end - $start" | bc)
printf "  sy:    %.3fs\n" "$sy_time"

# Restore file for rsync
cp "$PROFILE_DIR/src/large.bin" "$PROFILE_DIR/src/large_backup.bin"
dd if=/dev/urandom of="$PROFILE_DIR/src/large_backup.bin" bs=1048576 count=1 seek=50 conv=notrunc 2>/dev/null

# rsync delta sync
start=$(gdate +%s.%N)
rsync -a "$PROFILE_DIR/src/large_backup.bin" "$PROFILE_DIR/dst/large.bin"
end=$(gdate +%s.%N)
rsync_time=$(echo "$end - $start" | bc)
printf "  rsync: %.3fs\n" "$rsync_time"

speedup=$(echo "scale=2; $rsync_time / $sy_time" | bc)
printf "  Ratio: ${speedup}x\n"

echo ""
echo "🧹 Cleaning up..."
rm -rf "$PROFILE_DIR"
