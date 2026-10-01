#!/usr/bin/env bash
# ==============================================================================
# DustAgent Demo: Unix Pipeline Chaining
# Demonstrates STDIN / STDOUT composition with Unix tools (cat, jq, dust)
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DUST_BIN="$PROJECT_ROOT/target/debug/dust"

# 1. Ensure dust binary is compiled
if [[ ! -x "$DUST_BIN" ]]; then
    echo "[*] Compiling dust binary..."
    cargo build --bin dust --manifest-path "$PROJECT_ROOT/Cargo.toml"
fi

echo "======================================================================"
echo " [DustAgent] Unix Pipeline Demonstration"
echo " Zero-Chatter, Pure I/O, Sub-second Execution"
echo "======================================================================"

# Create temporary workspace
TMP_DIR=$(mktemp -d /tmp/dust-pipeline-demo-XXXXXX)
trap 'rm -rf "$TMP_DIR"' EXIT

SAMPLE_FILE="$TMP_DIR/worker.rs"
cat << 'EOF' > "$SAMPLE_FILE"
pub struct TaskWorker {
    pub max_retries: u32,
    pub is_active: bool,
}

impl TaskWorker {
    pub fn new() -> Self {
        Self {
            max_retries: 3,
            is_active: false,
        }
    }

    pub fn start(&mut self) {
        // Simple start routine
        self.is_active = true;
    }
}
EOF

echo ""
echo "1. Initial source file ($SAMPLE_FILE):"
cat "$SAMPLE_FILE"
echo ""

# 2. Simulate an in-place patch via `dust patch` dry-run
echo "2. Running 'dust patch' in dry-run mode:"
echo "   Instruction: 'Add a stop method that sets is_active to false'"
echo ""

# Note: In offline/mock mode or without OPENAI_API_KEY, we showcase the CLI flags and execution.
if [[ -z "${OPENAI_API_KEY:-}" ]]; then
    echo "[!] OPENAI_API_KEY is not set in environment."
    echo "    Showing how the pipeline command is structured for production:"
    echo ""
    echo "    # In-place patch command:"
    echo "    $DUST_BIN patch -f \"$SAMPLE_FILE\" -r \"15:20\" \"Add a stop() method\""
    echo ""
    echo "    # Or Unix pipe composition:"
    echo "    git diff | $DUST_BIN run commit_gen"
    echo ""
    echo "    For an immediate offline working demonstration, run:"
    echo "    cargo run --example fuzzy_patch_demo"
    echo "    cargo run --example unix_pipeline"
    echo "    cargo run --example subagent_orchestration"
else
    echo "[*] Executing live patch with OPENAI_API_KEY..."
    "$DUST_BIN" patch -f "$SAMPLE_FILE" --dry-run "Add a stop() method that sets is_active to false"
fi

echo ""
echo "======================================================================"
echo " Pipeline demo finished."
echo "======================================================================"
