#!/usr/bin/env bash
# Benchmark script for sy vs rsync

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BENCH_DIR="$PROJECT_ROOT/target/benchmark"

cd "$PROJECT_ROOT"

echo "📊 Benchmarking sy vs rsync"
echo "============================"
echo ""

# Build release binary
echo "📦 Building release binary..."
cargo build --release --quiet

# Create benchmark directory
mkdir -p "$BENCH_DIR"

# Function to run benchmark
run_benchmark() {
    local name="$1"
    local setup="$2"
    local sy_cmd="$3"
    local rsync_cmd="$4"

    echo ""
    echo "🏃 $name"
    echo "---"

    # Setup
    rm -rf "$BENCH_DIR/src" "$BENCH_DIR/dst"
    mkdir -p "$BENCH_DIR/src" "$BENCH_DIR/dst"
    eval "$setup"

    # Warmup
    "$PROJECT_ROOT/target/release/sy" "$BENCH_DIR/src" "$BENCH_DIR/dst" --quiet > /dev/null 2>&1 || true
    rm -rf "$BENCH_DIR/dst"
    mkdir -p "$BENCH_DIR/dst"

    # Run sy 3 times and take median
    local sy_times=()
    for i in {1..3}; do
        rm -rf "$BENCH_DIR/dst"
        mkdir -p "$BENCH_DIR/dst"
        local start=$(gdate +%s.%N)
        eval "$sy_cmd" > /dev/null 2>&1
        local end=$(gdate +%s.%N)
        local elapsed=$(echo "$end - $start" | bc)
        sy_times+=("$elapsed")
    done

    # Sort and get median
    IFS=$'\n' sy_times=($(sort -n <<<"${sy_times[*]}"))
    local sy_time="${sy_times[1]}"

    # Run rsync 3 times and take median
    local rsync_times=()
    for i in {1..3}; do
        rm -rf "$BENCH_DIR/dst"
        mkdir -p "$BENCH_DIR/dst"
        local start=$(gdate +%s.%N)
        eval "$rsync_cmd" > /dev/null 2>&1
        local end=$(gdate +%s.%N)
        local elapsed=$(echo "$end - $start" | bc)
        rsync_times+=("$elapsed")
    done

    # Sort and get median
    IFS=$'\n' rsync_times=($(sort -n <<<"${rsync_times[*]}"))
    local rsync_time="${rsync_times[1]}"

    # Calculate speedup
    local speedup=$(echo "scale=2; $rsync_time / $sy_time" | bc)

    printf "  sy:    %.3fs\n" "$sy_time"
    printf "  rsync: %.3fs\n" "$rsync_time"
    printf "  🚀 Speedup: ${speedup}x\n"
}

# Check for gdate (GNU date)
if ! command -v gdate &> /dev/null; then
    echo "⚠️  Installing GNU coreutils for precise timing..."
    brew install coreutils || {
        echo "❌ Failed to install coreutils. Using less precise timing."
        alias gdate=date
    }
fi

# Benchmark 1: Many small files (typical project)
run_benchmark \
    "1000 small files (1-10KB)" \
    "for i in {1..1000}; do dd if=/dev/urandom of=\"\$BENCH_DIR/src/file_\$i.txt\" bs=1024 count=\$((RANDOM % 10 + 1)) 2>/dev/null; done" \
    "$PROJECT_ROOT/target/release/sy \$BENCH_DIR/src \$BENCH_DIR/dst --quiet" \
    "rsync -a \$BENCH_DIR/src/ \$BENCH_DIR/dst/"

# Benchmark 2: 100 medium files (100KB each)
run_benchmark \
    "100 medium files (100KB)" \
    "for i in {1..100}; do dd if=/dev/urandom of=\"\$BENCH_DIR/src/file_\$i.dat\" bs=1024 count=100 2>/dev/null; done" \
    "$PROJECT_ROOT/target/release/sy \$BENCH_DIR/src \$BENCH_DIR/dst --quiet" \
    "rsync -a \$BENCH_DIR/src/ \$BENCH_DIR/dst/"

# Benchmark 3: Large file (100MB)
run_benchmark \
    "1 large file (100MB)" \
    "dd if=/dev/urandom of=\"\$BENCH_DIR/src/large.bin\" bs=1048576 count=100 2>/dev/null" \
    "$PROJECT_ROOT/target/release/sy \$BENCH_DIR/src/large.bin \$BENCH_DIR/dst/large.bin --quiet" \
    "rsync -a \$BENCH_DIR/src/large.bin \$BENCH_DIR/dst/large.bin"

# Benchmark 4: Deep directory tree
run_benchmark \
    "Deep directory tree (5 levels, 200 files)" \
    "for i in {1..5}; do mkdir -p \$BENCH_DIR/src/dir\$i; for j in {1..40}; do dd if=/dev/urandom of=\"\$BENCH_DIR/src/dir\$i/file_\$j.txt\" bs=1024 count=5 2>/dev/null; done; done" \
    "$PROJECT_ROOT/target/release/sy \$BENCH_DIR/src \$BENCH_DIR/dst --quiet" \
    "rsync -a \$BENCH_DIR/src/ \$BENCH_DIR/dst/"

# Benchmark 5: Delta sync (1MB change in 100MB file)
echo ""
echo "🏃 Delta sync (1MB change in 100MB)"
echo "---"
rm -rf "$BENCH_DIR/src" "$BENCH_DIR/dst"
mkdir -p "$BENCH_DIR/src" "$BENCH_DIR/dst"
dd if=/dev/urandom of="$BENCH_DIR/src/large.bin" bs=1048576 count=100 2>/dev/null

# Initial sync
"$PROJECT_ROOT/target/release/sy" "$BENCH_DIR/src/large.bin" "$BENCH_DIR/dst/large.bin" --quiet
cp "$BENCH_DIR/src/large.bin" "$BENCH_DIR/dst/rsync_large.bin"

# Modify 1MB in the middle
dd if=/dev/urandom of="$BENCH_DIR/src/large.bin" bs=1048576 count=1 seek=50 conv=notrunc 2>/dev/null

# sy delta sync (3 runs)
sy_times=()
for i in {1..3}; do
    cp "$BENCH_DIR/dst/large.bin" "$BENCH_DIR/dst/large_backup.bin"
    start=$(gdate +%s.%N)
    "$PROJECT_ROOT/target/release/sy" "$BENCH_DIR/src/large.bin" "$BENCH_DIR/dst/large_backup.bin" --quiet
    end=$(gdate +%s.%N)
    elapsed=$(echo "$end - $start" | bc)
    sy_times+=("$elapsed")
    rm "$BENCH_DIR/dst/large_backup.bin"
done
IFS=$'\n' sy_times=($(sort -n <<<"${sy_times[*]}"))
sy_time="${sy_times[1]}"

# rsync delta sync (3 runs)
rsync_times=()
for i in {1..3}; do
    cp "$BENCH_DIR/dst/rsync_large.bin" "$BENCH_DIR/dst/rsync_large_backup.bin"
    start=$(gdate +%s.%N)
    rsync -a --ignore-times "$BENCH_DIR/src/large.bin" "$BENCH_DIR/dst/rsync_large_backup.bin"
    end=$(gdate +%s.%N)
    elapsed=$(echo "$end - $start" | bc)
    rsync_times+=("$elapsed")
    rm "$BENCH_DIR/dst/rsync_large_backup.bin"
done
IFS=$'\n' rsync_times=($(sort -n <<<"${rsync_times[*]}"))
rsync_time="${rsync_times[1]}"

speedup=$(echo "scale=2; $rsync_time / $sy_time" | bc)
printf "  sy:    %.3fs\n" "$sy_time"
printf "  rsync: %.3fs\n" "$rsync_time"
printf "  🚀 Speedup: ${speedup}x\n"

echo ""
echo "✅ Benchmarking complete!"
echo ""
echo "🧹 Cleaning up..."
rm -rf "$BENCH_DIR"
