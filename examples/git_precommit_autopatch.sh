#!/usr/bin/env bash
# ==============================================================================
# DustAgent Demo: Git Pre-Commit Auto-Patch Hook
# Demonstrates Zero-Interaction automated self-healing workflows
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DUST_BIN="$PROJECT_ROOT/target/debug/dust"

echo "======================================================================"
echo " [DustAgent] Pre-Commit Auto-Patch Hook Simulation"
echo " Concept: Compiler Fail -> Diagnostician -> Patcher -> Git Commit"
echo "======================================================================"

TMP_DIR=$(mktemp -d /tmp/dust-precommit-XXXXXX)
trap 'rm -rf "$TMP_DIR"' EXIT

TARGET="$TMP_DIR/lib.rs"
cat << 'EOF' > "$TARGET"
pub fn add_numbers(a: i32, b: i32) -> i32 {
    // Unused mut warning trigger
    let mut result = a + b;
    result
}
EOF

echo "1. Staged file contains a compiler warning/issue:"
cat "$TARGET"
echo ""

echo "2. Simulating pre-commit check command:"
echo "   $ cargo check 2> errors.log || true"
echo "   Warning detected: 'variable does not need to be mutable: mut result'"
echo ""

echo "3. Automated Auto-Fix Command (Zero-Interaction):"
echo "   dust patch -f $TARGET \"Remove unused mut keyword from variable result\""
echo ""

echo "4. Under the hood:"
echo "   - No conversational questions ('Do you want me to apply this? [y/N]')"
echo "   - In-place atomic patch in < 5ms"
echo "   - Leaves workspace 100% clean for git commit"
echo "======================================================================"
