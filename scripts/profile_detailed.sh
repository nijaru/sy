#!/usr/bin/env bash
# Detailed profiling of delta sync

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_ROOT"

echo "🔍 Detailed Delta Sync Profiling"
echo "=================================="
echo ""

# Build release binary
echo "📦 Building release binary..."
cargo build --release --quiet

# Create test data
TEST_DIR="/tmp/sy_profile"
rm -rf "$TEST_DIR"
mkdir -p "$TEST_DIR"

echo "📝 Creating 100MB test file..."
dd if=/dev/urandom of="$TEST_DIR/test.bin" bs=1048576 count=100 2>/dev/null

echo "📋 Initial sync..."
"$PROJECT_ROOT/target/release/sy" "$TEST_DIR/test.bin" "$TEST_DIR/test_dst.bin" --quiet

echo "✏️  Modifying 1MB in the middle..."
dd if=/dev/urandom of="$TEST_DIR/test.bin" bs=1048576 count=1 seek=50 conv=notrunc 2>/dev/null

echo ""
echo "🔥 Profiling delta sync with samply..."
echo ""

# Run with samply
samply record --save-only --output "$TEST_DIR/profile.json" -- \
    "$PROJECT_ROOT/target/release/sy" "$TEST_DIR/test.bin" "$TEST_DIR/test_dst.bin" --quiet

echo ""
echo "✅ Profile saved to: $TEST_DIR/profile.json"
echo "   View with: samply load $TEST_DIR/profile.json"
echo ""

# Also do a manual timing breakdown with RUST_LOG
echo "📊 Timing breakdown (with logging):"
echo ""
RUST_LOG=info "$PROJECT_ROOT/target/release/sy" "$TEST_DIR/test.bin" "$TEST_DIR/test_dst.bin" 2>&1 | grep -E "INFO|Delta|Computing|Generating|Applying"

echo ""
echo "🧹 To clean up: rm -rf $TEST_DIR"
