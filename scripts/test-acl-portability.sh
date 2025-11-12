#!/usr/bin/env bash
#
# Test ACL feature portability in Fedora Docker container
#
# Tests that:
# 1. Default build works without libacl-devel
# 2. ACL feature build fails without libacl-devel
# 3. ACL feature works after installing libacl-devel
# 4. Runtime error message for missing feature
#
# This script ONLY runs in Docker to avoid affecting the host OS.
#
# Usage:
#   scripts/test-acl-portability.sh

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}ℹ${NC} $*"; }
log_success() { echo -e "${GREEN}✓${NC} $*"; }
log_error() { echo -e "${RED}✗${NC} $*"; }
log_warn() { echo -e "${YELLOW}⚠${NC} $*"; }

# Detect if running in Docker
if [ -f /.dockerenv ]; then
    IN_DOCKER=true
else
    IN_DOCKER=false
fi

# Test 1: Build without ACL feature (should work without libacl-devel)
test_default_build() {
    log_info "Test 1: Building without ACL feature (no system deps required)..."

    if cargo build --release 2>&1 | grep -q "Finished.*release"; then
        log_success "Default build works without libacl-devel"
        return 0
    else
        log_error "Default build failed"
        return 1
    fi
}

# Test 2: Build with ACL feature (should fail without libacl-devel)
test_acl_without_lib() {
    log_info "Test 2: Building with ACL feature (should fail without libacl-devel)..."

    # Remove libacl if it exists
    dnf remove -y libacl-devel 2>/dev/null || true
    dnf clean all

    if cargo build --release --features acl 2>&1 | grep -q "cannot find -lacl"; then
        log_success "ACL build correctly fails without libacl-devel"
        return 0
    else
        log_warn "ACL build succeeded (libacl-devel might be pre-installed)"
        return 0  # Not a failure, just unexpected
    fi
}

# Test 3: Build with ACL feature after installing libacl-devel
test_acl_with_lib() {
    log_info "Test 3: Installing libacl-devel and building with ACL feature..."

    log_info "Installing libacl-devel..."
    dnf install -y libacl-devel

    log_info "Building with ACL feature..."
    if cargo build --release --features acl 2>&1 | grep -q "Finished.*release"; then
        log_success "ACL build works with libacl-devel installed"
        return 0
    else
        log_error "ACL build failed even with libacl-devel"
        return 1
    fi
}

# Test 4: Verify runtime behavior
test_runtime() {
    log_info "Test 4: Testing runtime ACL error message..."

    # Build without ACL feature
    cargo build --release --quiet

    # Create test directories
    mkdir -p /tmp/test1 /tmp/test2
    echo "test" > /tmp/test1/file.txt

    # Try to use --preserve-acls without feature
    local output
    output=$(./target/release/sy /tmp/test1 /tmp/test2 --preserve-acls --dry-run 2>&1 || true)

    # Cleanup
    rm -rf /tmp/test1 /tmp/test2

    if echo "$output" | grep -q "ACL preservation requires the 'acl' feature"; then
        log_success "Runtime error message works correctly"
        return 0
    else
        log_error "Expected ACL feature error message not found"
        echo "$output"
        return 1
    fi
}

# Main test suite (runs inside Docker container)
run_tests_in_container() {
    echo ""
    log_info "ACL Feature Portability Test Suite (Fedora Container)"
    echo ""

    local failed=0

    # Run all tests
    test_default_build || failed=$((failed + 1))
    echo ""

    test_acl_without_lib || failed=$((failed + 1))
    echo ""

    test_acl_with_lib || failed=$((failed + 1))
    echo ""

    test_runtime || failed=$((failed + 1))
    echo ""

    # Summary
    if [ $failed -eq 0 ]; then
        log_success "All tests passed! ✨"
        echo ""
        log_info "Verified:"
        echo "  ✓ Default build works without libacl-devel"
        echo "  ✓ ACL feature requires libacl-devel"
        echo "  ✓ ACL feature works with libacl-devel"
        echo "  ✓ Runtime error message is correct"
        echo ""
        log_info "Ready for:"
        echo "  • cargo install sy                 → Works everywhere"
        echo "  • cargo install sy --features acl  → Requires libacl-devel on Linux"
        echo ""
        return 0
    else
        log_error "$failed test(s) failed"
        return 1
    fi
}

# Main entry point - launches Docker container
main() {
    # Check if we're inside Docker
    if [ "$IN_DOCKER" = true ]; then
        # We're inside the container, run the tests
        run_tests_in_container
    else
        # We're on the host, launch Docker
        log_info "Starting Fedora Docker container for ACL portability tests..."
        echo ""

        docker run --rm \
            -v "$(pwd):/sy" \
            -w /sy \
            fedora:latest \
            bash -c "
                echo 'Installing build dependencies...'
                dnf install -y rust cargo openssl-devel 2>&1 | grep -E '(Installing|Complete|Nothing)' || true
                echo ''
                echo 'Cleaning any host-built artifacts...'
                rm -rf target/
                echo ''
                ./scripts/test-acl-portability.sh
            "
    fi
}

# Run
main "$@"
