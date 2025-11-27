#!/bin/bash
set -e

# Configuration
REMOTE_HOST="fedora"
REPO_URL="https://github.com/torvalds/linux"
# Use a smaller depth/branch if full linux kernel is too much, but depth 1 is standard for "large repo" test
# Linux kernel is ~1GB source. 
# Alternatives: 
# - https://github.com/bevyengine/bevy (smaller, ~100MB)
# - https://github.com/tokio-rs/tokio (small, ~20MB)
# Let's use bevy for a "medium" test that is significant but not 1GB+
TEST_REPO_URL="https://github.com/bevyengine/bevy" 
LOCAL_BASE="tmp"
DATASET_NAME="bench_dataset"
LOCAL_DIR="$LOCAL_BASE/$DATASET_NAME"
REMOTE_BASE="~/sy_bench"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'

log() {
    echo -e "${GREEN}[BENCH] $1${NC}"
}

error() {
    echo -e "${RED}[ERROR] $1${NC}"
}

# 1. Build Local Release
log "Building local sy (release)..."
cargo build --release

# 2. Update Remote sy
log "Updating remote sy on $REMOTE_HOST..."
ssh $REMOTE_HOST "bash -s" <<EOF
    set -e
    cd ~/github/nijaru/sy
    echo "  Pulling latest changes..."
    git fetch origin
    git reset --hard origin/main
    echo "  Installing sy..."
    cargo install --path . --quiet
    echo "  Remote sy version: \$(sy --version)"
EOF

# 3. Prepare Test Data
if [ ! -d "$LOCAL_DIR" ]; then
    log "Cloning test dataset ($TEST_REPO_URL)..."
    mkdir -p $LOCAL_BASE
    git clone --depth 1 $TEST_REPO_URL $LOCAL_DIR
else
    log "Test dataset exists ($LOCAL_DIR)"
fi

# Calculate dataset size
FILE_COUNT=$(find $LOCAL_DIR -type f | wc -l)
TOTAL_SIZE=$(du -sh $LOCAL_DIR | cut -f1)
log "Dataset: $DATASET_NAME | Files: $FILE_COUNT | Size: $TOTAL_SIZE"

# 4. Clean Remote Targets
log "Cleaning remote targets..."
ssh $REMOTE_HOST "rm -rf $REMOTE_BASE"
ssh $REMOTE_HOST "mkdir -p $REMOTE_BASE"

# 5. Benchmark Rsync
log "--- Running rsync (Baseline) ---"
# Sync to ~/sy_bench/rsync/bench_dataset
TIME_RSYNC=$( { time rsync -a $LOCAL_DIR $REMOTE_HOST:$REMOTE_BASE/rsync/ > /dev/null; } 2>&1 )
echo "$TIME_RSYNC"

# 6. Benchmark Sy Server Mode
log "--- Running sy --server (Experimental) ---"
# Sync to ~/sy_bench/sy/bench_dataset
# We use the locally built release binary
# SY_USE_SERVER=1 triggers the client-side server logic
export SY_USE_SERVER=1
TIME_SY=$( { time ./target/release/sy $LOCAL_DIR $REMOTE_HOST:$REMOTE_BASE/sy/ > /dev/null; } 2>&1 )
echo "$TIME_SY"

# 7. Verify Integrity
log "Verifying integrity..."
# We use diff -r. Note: rsync -a preserves timestamps, sy -server (Phase 1) might mostly do it (we parse mtime) 
# but diff might complain about timestamps if we don't ignore them.
# Let's check if files differ.
if ssh $REMOTE_HOST "/usr/bin/diff -r -q $REMOTE_BASE/rsync/$DATASET_NAME $REMOTE_BASE/sy/$DATASET_NAME"; then
    log "SUCCESS: Directories are identical."
else
    error "FAILURE: Directories differ!"
    exit 1
fi

log "Benchmark Complete."
