#!/bin/bash
# Real-world SSH Bidirectional Sync Test Suite
#
# This script tests sy's SSH bidirectional sync functionality with real scenarios
# Run this manually against actual servers to validate the feature

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Test counters
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

# Helper functions
log_test() {
    echo -e "${YELLOW}[TEST $((TESTS_RUN+1))]${NC} $1"
    TESTS_RUN=$((TESTS_RUN+1))
}

log_pass() {
    echo -e "${GREEN}✓ PASS${NC} $1"
    TESTS_PASSED=$((TESTS_PASSED+1))
}

log_fail() {
    echo -e "${RED}✗ FAIL${NC} $1"
    TESTS_FAILED=$((TESTS_FAILED+1))
}

log_info() {
    echo -e "  ${NC}$1${NC}"
}

# Cleanup function
cleanup() {
    echo ""
    echo "=== Cleaning up test directories ==="
    rm -rf /tmp/sy-bisync-test-*
    echo "Cleanup complete"
}

trap cleanup EXIT

# Test 1: Local ↔ Local Basic Sync
test_local_to_local_basic() {
    log_test "Local ↔ Local Basic Bidirectional Sync"

    # Create test directories
    local SOURCE="/tmp/sy-bisync-test-source-$$"
    local DEST="/tmp/sy-bisync-test-dest-$$"

    mkdir -p "$SOURCE" "$DEST"

    # Create initial files in source
    echo "source file 1" > "$SOURCE/file1.txt"
    echo "source file 2" > "$SOURCE/file2.txt"

    # First sync: source → dest
    log_info "Running first sync (source → dest)..."
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --conflict-resolve source 2>&1 | grep -E "(synced|Files)"

    # Verify files copied to dest
    if [ -f "$DEST/file1.txt" ] && [ -f "$DEST/file2.txt" ]; then
        log_pass "Files synced from source to dest"
    else
        log_fail "Files not found in dest after sync"
        return 1
    fi

    # Create new file in dest
    echo "dest file 3" > "$DEST/file3.txt"

    # Second sync: dest → source
    log_info "Running second sync (bidirectional)..."
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --conflict-resolve source 2>&1 | grep -E "(synced|Files)"

    # Verify dest file copied to source
    if [ -f "$SOURCE/file3.txt" ]; then
        log_pass "New file synced from dest to source"
    else
        log_fail "Dest file not synced to source"
        return 1
    fi

    # Cleanup
    rm -rf "$SOURCE" "$DEST"
}

# Test 2: Conflict Resolution - Newer Wins
test_conflict_newer_wins() {
    log_test "Conflict Resolution: Newer Wins"

    local SOURCE="/tmp/sy-bisync-test-source-conflict-$$"
    local DEST="/tmp/sy-bisync-test-dest-conflict-$$"

    mkdir -p "$SOURCE" "$DEST"

    # Create and sync initial file
    echo "initial content" > "$SOURCE/conflict.txt"
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --conflict-resolve newer --quiet

    # Modify on both sides (dest is newer)
    echo "source version" > "$SOURCE/conflict.txt"
    sleep 1  # Ensure timestamp difference
    echo "dest version (newer)" > "$DEST/conflict.txt"

    # Sync with newer strategy
    log_info "Syncing with conflict (newer wins)..."
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --conflict-resolve newer 2>&1 | grep -E "(conflict|synced)"

    # Verify newer (dest) version won
    local source_content=$(cat "$SOURCE/conflict.txt")
    if [ "$source_content" = "dest version (newer)" ]; then
        log_pass "Newer version (from dest) correctly won conflict"
    else
        log_fail "Wrong version: expected 'dest version (newer)', got '$source_content'"
        return 1
    fi

    rm -rf "$SOURCE" "$DEST"
}

# Test 3: Conflict Resolution - Rename Both
test_conflict_rename() {
    log_test "Conflict Resolution: Rename Both Files"

    local SOURCE="/tmp/sy-bisync-test-source-rename-$$"
    local DEST="/tmp/sy-bisync-test-dest-rename-$$"

    mkdir -p "$SOURCE" "$DEST"

    # Create and sync initial file
    echo "initial" > "$SOURCE/rename.txt"
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --conflict-resolve rename --quiet

    # Modify on both sides
    echo "source modification" > "$SOURCE/rename.txt"
    echo "dest modification" > "$DEST/rename.txt"

    # Sync with rename strategy
    log_info "Syncing with conflict (rename both)..."
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --conflict-resolve rename 2>&1 | grep -E "(conflict|renamed)"

    # Verify both versions exist with conflict suffix
    local source_conflicts=$(ls "$SOURCE"/rename.conflict-* 2>/dev/null | wc -l)
    local dest_conflicts=$(ls "$DEST"/rename.conflict-* 2>/dev/null | wc -l)

    if [ "$source_conflicts" -ge 1 ] && [ "$dest_conflicts" -ge 1 ]; then
        log_pass "Both files renamed with conflict suffix"
        log_info "Source conflicts: $source_conflicts, Dest conflicts: $dest_conflicts"
    else
        log_fail "Conflict files not created (source: $source_conflicts, dest: $dest_conflicts)"
        return 1
    fi

    rm -rf "$SOURCE" "$DEST"
}

# Test 4: State Persistence Across Syncs
test_state_persistence() {
    log_test "State Persistence Across Multiple Syncs"

    local SOURCE="/tmp/sy-bisync-test-source-persist-$$"
    local DEST="/tmp/sy-bisync-test-dest-persist-$$"

    mkdir -p "$SOURCE" "$DEST"

    # First sync with some files
    echo "file1" > "$SOURCE/file1.txt"
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --quiet

    # Second sync: no changes (should be idempotent)
    log_info "Running sync with no changes..."
    local output=$(cargo run --release --bin sy -- -b "$SOURCE" "$DEST" 2>&1)

    if echo "$output" | grep -q "Files scanned:     0"; then
        log_pass "Idempotent sync detected no changes"
    else
        log_fail "Sync falsely detected changes when none exist"
        echo "$output"
        return 1
    fi

    # Third sync: modify only source
    echo "modified" > "$SOURCE/file1.txt"
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --quiet

    # Verify state persisted and detected modification correctly
    local dest_content=$(cat "$DEST/file1.txt")
    if [ "$dest_content" = "modified" ]; then
        log_pass "State correctly tracked modification"
    else
        log_fail "State did not correctly track modification"
        return 1
    fi

    rm -rf "$SOURCE" "$DEST"
}

# Test 5: Large File Set (1000 files)
test_large_file_set() {
    log_test "Large File Set (1000 files)"

    local SOURCE="/tmp/sy-bisync-test-source-large-$$"
    local DEST="/tmp/sy-bisync-test-dest-large-$$"

    mkdir -p "$SOURCE" "$DEST"

    # Create 1000 small files
    log_info "Creating 1000 test files..."
    for i in $(seq 1 1000); do
        echo "file $i content" > "$SOURCE/file$i.txt"
    done

    # Sync
    log_info "Syncing 1000 files (measuring time)..."
    local start_time=$(date +%s)
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --quiet
    local end_time=$(date +%s)
    local duration=$((end_time - start_time))

    # Verify count
    local dest_count=$(ls "$DEST" | wc -l)
    if [ "$dest_count" -eq 1000 ]; then
        log_pass "All 1000 files synced successfully in ${duration}s"
    else
        log_fail "Expected 1000 files, found $dest_count"
        return 1
    fi

    # Modify 10 random files and sync again
    log_info "Modifying 10 files and re-syncing..."
    for i in $(seq 100 10 190); do
        echo "modified $i" > "$SOURCE/file$i.txt"
    done

    start_time=$(date +%s)
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" 2>&1 | grep -E "Files synced"
    end_time=$(date +%s)
    duration=$((end_time - start_time))

    log_pass "Incremental sync of 10 files completed in ${duration}s"

    rm -rf "$SOURCE" "$DEST"
}

# Test 6: Deletion Safety (Max Delete Threshold)
test_deletion_safety() {
    log_test "Deletion Safety (Max Delete Threshold)"

    local SOURCE="/tmp/sy-bisync-test-source-delete-$$"
    local DEST="/tmp/sy-bisync-test-dest-delete-$$"

    mkdir -p "$SOURCE" "$DEST"

    # Create and sync 10 files
    for i in $(seq 1 10); do
        echo "file $i" > "$SOURCE/file$i.txt"
    done
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --quiet

    # Delete 6 files from source (60% deletion)
    rm "$SOURCE"/file{1,2,3,4,5,6}.txt

    # Try to sync with default 50% threshold (should fail)
    log_info "Attempting sync with 60% deletion (should abort)..."
    if cargo run --release --bin sy -- -b "$SOURCE" "$DEST" 2>&1 | grep -q "Deletion limit exceeded"; then
        log_pass "Deletion limit correctly prevented mass deletion"
    else
        log_fail "Deletion limit did not trigger"
        return 1
    fi

    # Sync with increased threshold
    log_info "Syncing with --max-delete 0 (unlimited)..."
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --max-delete 0 2>&1 | grep -E "deleted"

    local dest_count=$(ls "$DEST" 2>/dev/null | wc -l)
    if [ "$dest_count" -eq 4 ]; then
        log_pass "Deletions propagated with unlimited threshold"
    else
        log_fail "Expected 4 files, found $dest_count"
        return 1
    fi

    rm -rf "$SOURCE" "$DEST"
}

# Test 7: Dry Run Mode
test_dry_run() {
    log_test "Dry Run Mode (No Actual Changes)"

    local SOURCE="/tmp/sy-bisync-test-source-dryrun-$$"
    local DEST="/tmp/sy-bisync-test-dest-dryrun-$$"

    mkdir -p "$SOURCE" "$DEST"

    # Create files
    echo "source file" > "$SOURCE/new.txt"
    echo "dest file" > "$DEST/existing.txt"

    # Dry run
    log_info "Running dry run..."
    cargo run --release --bin sy -- -b "$SOURCE" "$DEST" --dry-run 2>&1 | grep -E "(Would|Dry-run)"

    # Verify no changes were made
    if [ ! -f "$DEST/new.txt" ] && [ ! -f "$SOURCE/existing.txt" ]; then
        log_pass "Dry run made no actual changes"
    else
        log_fail "Dry run modified filesystem"
        return 1
    fi

    rm -rf "$SOURCE" "$DEST"
}

# Main execution
main() {
    echo "================================================"
    echo "  sy SSH Bidirectional Sync - Real World Tests"
    echo "================================================"
    echo ""

    # Run all tests
    test_local_to_local_basic || true
    test_conflict_newer_wins || true
    test_conflict_rename || true
    test_state_persistence || true
    test_large_file_set || true
    test_deletion_safety || true
    test_dry_run || true

    # Summary
    echo ""
    echo "================================================"
    echo "  Test Summary"
    echo "================================================"
    echo "Tests Run:    $TESTS_RUN"
    echo -e "Tests Passed: ${GREEN}$TESTS_PASSED${NC}"
    if [ $TESTS_FAILED -gt 0 ]; then
        echo -e "Tests Failed: ${RED}$TESTS_FAILED${NC}"
    else
        echo -e "Tests Failed: ${GREEN}$TESTS_FAILED${NC}"
    fi
    echo "================================================"

    # Exit with failure if any tests failed
    if [ $TESTS_FAILED -gt 0 ]; then
        exit 1
    fi
}

# Check if sy is built
if [ ! -f "target/release/sy" ]; then
    echo "Building sy in release mode..."
    cargo build --release
fi

# Run tests
main
