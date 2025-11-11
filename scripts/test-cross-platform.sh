#!/usr/bin/env bash
#
# Cross-platform testing script for sy (macOS ↔ Fedora)
#
# Automatically tests the current branch on both macOS and Fedora.
# Builds sy on macOS, builds/installs sy-remote on Fedora, runs SSH tests.
#
# Usage:
#   scripts/test-cross-platform.sh           # Basic output
#   scripts/test-cross-platform.sh --verbose # Full logging
#
# Prerequisites:
# - SSH access to fedora (nick@fedora via tailscale)
# - Git repo will be auto-cloned on Fedora if missing

set -euo pipefail

# Configuration
FEDORA_HOST="fedora"
FEDORA_USER="nick"
FEDORA_REPO_PATH="~/github/nijaru/sy"
VERBOSE=false
CURRENT_BRANCH=""

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Logging functions
log_info() {
    echo -e "${BLUE}ℹ${NC} $*"
}

log_success() {
    echo -e "${GREEN}✓${NC} $*"
}

log_error() {
    echo -e "${RED}✗${NC} $*"
}

log_warn() {
    echo -e "${YELLOW}⚠${NC} $*"
}

log_verbose() {
    if [[ "$VERBOSE" == true ]]; then
        echo -e "${NC}  $*${NC}"
    fi
}

# Parse arguments
if [[ $# -gt 0 ]]; then
    if [[ "$1" == "--verbose" ]]; then
        VERBOSE=true
    else
        log_error "Invalid argument: $1"
        echo "Usage: $0 [--verbose]"
        exit 1
    fi
fi

# Run command with optional verbose logging
run_cmd() {
    local cmd="$*"
    log_verbose "Running: $cmd"

    if [[ "$VERBOSE" == true ]]; then
        eval "$cmd"
    else
        eval "$cmd" >/dev/null 2>&1
    fi
}

# Check SSH connectivity
check_fedora_ssh() {
    log_info "Checking SSH connection to $FEDORA_USER@$FEDORA_HOST..."
    if ssh -o ConnectTimeout=5 -o BatchMode=yes "$FEDORA_USER@$FEDORA_HOST" "exit" 2>/dev/null; then
        log_success "SSH connection OK"
        return 0
    else
        log_error "Cannot connect to $FEDORA_USER@$FEDORA_HOST"
        log_error "Make sure fedora is accessible via SSH (tailscale)"
        return 1
    fi
}

# Build sy locally (macOS)
build_macos() {
    log_info "Building sy on macOS..."

    # Detect current branch
    CURRENT_BRANCH=$(git branch --show-current)
    log_info "Testing branch: $CURRENT_BRANCH"

    # Fetch latest from origin
    log_verbose "Fetching latest from origin"
    run_cmd "git fetch origin"

    # Build sy and sy-remote
    log_verbose "cargo build --release --bin sy"
    if [[ "$VERBOSE" == true ]]; then
        cargo build --release --bin sy 2>&1
    else
        cargo build --release --bin sy 2>&1 | grep -E "(Compiling sy|Finished)" || true
    fi

    log_verbose "cargo build --release --bin sy-remote"
    if [[ "$VERBOSE" == true ]]; then
        cargo build --release --bin sy-remote 2>&1
    else
        cargo build --release --bin sy-remote 2>&1 | grep -E "(Compiling sy|Finished)" || true
    fi

    log_success "macOS build complete"
}

# Setup sy on Fedora
setup_fedora() {
    log_info "Setting up sy on Fedora..."
    log_info "Using branch: $CURRENT_BRANCH"

    # Check if repo exists, clone if missing
    log_verbose "Checking if repo exists on Fedora"
    if ! ssh "$FEDORA_USER@$FEDORA_HOST" "test -d $FEDORA_REPO_PATH/.git" 2>/dev/null; then
        log_warn "Repository not found, cloning from GitHub..."
        if ! ssh "$FEDORA_USER@$FEDORA_HOST" "mkdir -p $(dirname $FEDORA_REPO_PATH) && git clone https://github.com/nijaru/sy $FEDORA_REPO_PATH" 2>&1 | while read -r line; do
            log_verbose "$line"
        done; then
            log_error "Failed to clone repository"
            return 1
        fi
        log_success "Repository cloned successfully"
    fi

    # Update to same branch as macOS
    log_verbose "Syncing to branch: $CURRENT_BRANCH"
    if ! ssh "$FEDORA_USER@$FEDORA_HOST" "cd $FEDORA_REPO_PATH && git fetch origin && git checkout $CURRENT_BRANCH && git pull origin $CURRENT_BRANCH" 2>&1 | while read -r line; do
        log_verbose "$line"
    done; then
        log_error "Failed to update git repo on Fedora"
        log_error "Check that branch '$CURRENT_BRANCH' exists on origin"
        return 1
    fi

    # Build sy-remote on Fedora
    log_verbose "Building sy-remote on Fedora"
    if [[ "$VERBOSE" == true ]]; then
        if ! ssh "$FEDORA_USER@$FEDORA_HOST" "cd $FEDORA_REPO_PATH && cargo build --release --bin sy-remote" 2>&1; then
            log_error "Build failed on Fedora"
            return 1
        fi
    else
        if ! ssh "$FEDORA_USER@$FEDORA_HOST" "cd $FEDORA_REPO_PATH && cargo build --release --bin sy-remote" 2>&1 | grep -E "(Compiling sy|Finished)" || true; then
            log_error "Build may have failed on Fedora (run with --verbose to see details)"
            return 1
        fi
    fi

    # Install sy-remote to ~/.cargo/bin on Fedora (overwrites existing)
    log_verbose "Installing sy-remote on Fedora (will overwrite if already installed)"
    if ! ssh "$FEDORA_USER@$FEDORA_HOST" "cd $FEDORA_REPO_PATH && cargo install --path . --bin sy-remote" 2>&1 | while read -r line; do
        log_verbose "$line"
    done; then
        log_error "Installation failed on Fedora"
        return 1
    fi

    # Verify sy-remote is accessible
    log_verbose "Verifying sy-remote installation"
    if ! ssh "$FEDORA_USER@$FEDORA_HOST" "command -v sy-remote >/dev/null 2>&1" 2>/dev/null; then
        log_error "sy-remote not found in PATH after installation"
        log_error "Ensure ~/.cargo/bin is in PATH on Fedora"
        return 1
    fi

    log_success "Fedora setup complete"
}

# Run comprehensive SSH tests
run_tests() {
    log_info "Running comprehensive SSH tests..."

    local test_output
    local exit_code=0

    if [[ "$VERBOSE" == true ]]; then
        log_verbose "cargo test --test ssh_comprehensive_test -- --ignored --nocapture"
        cargo test --test ssh_comprehensive_test -- --ignored --nocapture 2>&1 || exit_code=$?
    else
        test_output=$(cargo test --test ssh_comprehensive_test -- --ignored 2>&1) || exit_code=$?

        # Parse results
        local passed=$(echo "$test_output" | grep -oE "[0-9]+ passed" | grep -oE "[0-9]+")
        local failed=$(echo "$test_output" | grep -oE "[0-9]+ failed" | grep -oE "[0-9]+")
        local ignored=$(echo "$test_output" | grep -oE "[0-9]+ ignored" | grep -oE "[0-9]+")

        echo ""
        log_info "Test Results:"
        [[ -n "$passed" ]] && log_success "Passed: $passed"
        [[ -n "$failed" && "$failed" != "0" ]] && log_error "Failed: $failed"
        [[ -n "$ignored" && "$ignored" != "0" ]] && log_warn "Ignored: $ignored"
        echo ""

        # Show failures if any
        if [[ "$failed" != "0" && -n "$failed" ]]; then
            log_error "Test failures detected. Run with --verbose for details:"
            echo "$test_output" | grep -A 50 "failures:" || true
        fi
    fi

    return $exit_code
}

# Cleanup function
cleanup() {
    # No-op - stay on the branch we were testing
    :
}

# Main execution
main() {
    echo ""
    log_info "Cross-Platform Test Suite for sy (macOS ↔ Fedora)"
    echo ""

    # Check prerequisites
    check_fedora_ssh || exit 1

    # Build on both platforms
    build_macos
    setup_fedora

    echo ""

    # Run tests
    local test_result=0
    run_tests || test_result=$?

    echo ""

    if [[ $test_result -eq 0 ]]; then
        log_success "All tests passed! ✨"
        echo ""
        log_info "Branch '$CURRENT_BRANCH' is ready for:"
        echo "  1. Pushing to origin (if not already pushed)"
        echo "  2. Opening/updating PR on GitHub"
        echo "  3. Merging after CI passes"
        echo ""
    else
        log_error "Some tests failed. Review output above."
        echo ""
        log_info "To debug:"
        echo "  scripts/test-cross-platform.sh --verbose"
        echo ""
        exit 1
    fi

    # Cleanup
    cleanup
}

# Trap errors and cleanup
trap cleanup EXIT

# Run main
main "$@"
