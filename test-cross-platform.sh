#!/usr/bin/env bash
#
# Cross-platform testing script for sy (macOS ↔ Fedora)
#
# Usage:
#   ./test-cross-platform.sh           # Basic output
#   ./test-cross-platform.sh --verbose # Full logging
#
# Prerequisites:
# - SSH access to fedora (nick@fedora via tailscale)
# - Git repo cloned on both machines

set -euo pipefail

# Configuration
FEDORA_HOST="fedora"
FEDORA_USER="nick"
FEDORA_REPO_PATH="~/github/nijaru/sy"
VERBOSE=false

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Parse arguments
if [[ "${1:-}" == "--verbose" ]]; then
    VERBOSE=true
fi

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

    # Switch to feature branch
    log_verbose "Checking out feature/library-migrations-v0.0.58"
    run_cmd "git fetch origin"
    run_cmd "git checkout feature/library-migrations-v0.0.58"

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

    # Check if repo exists
    log_verbose "Checking if repo exists on Fedora"
    if ! ssh "$FEDORA_USER@$FEDORA_HOST" "test -d $FEDORA_REPO_PATH/.git" 2>/dev/null; then
        log_error "Repository not found at $FEDORA_REPO_PATH on Fedora"
        log_error "Please clone the repo first:"
        echo "  ssh $FEDORA_USER@$FEDORA_HOST \"git clone https://github.com/nijaru/sy $FEDORA_REPO_PATH\""
        return 1
    fi

    # Pull latest code on Fedora
    log_verbose "Pulling latest code on Fedora"
    if ! ssh "$FEDORA_USER@$FEDORA_HOST" "cd $FEDORA_REPO_PATH && git fetch origin && git checkout feature/library-migrations-v0.0.58 && git pull origin feature/library-migrations-v0.0.58" 2>&1 | while read -r line; do
        log_verbose "$line"
    done; then
        log_error "Failed to update git repo on Fedora"
        log_error "Check that the branch exists: git fetch origin"
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
    log_verbose "Switching back to main branch"
    git checkout main 2>/dev/null || true
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
        log_info "Next steps:"
        echo "  1. Review test results above"
        echo "  2. Check CI status on GitHub"
        echo "  3. Merge PR #6 if everything looks good"
        echo ""
    else
        log_error "Some tests failed. Review output above."
        echo ""
        log_info "To debug:"
        echo "  ./test-cross-platform.sh --verbose"
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
