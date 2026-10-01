# Contributing to DustAgent

Thank you for your interest in contributing to DustAgent!

DustAgent is built around the **Unix Philosophy**: *"Do one thing and do it well, communicate via clean streams, and eliminate framework bloat."*

---

## 🛠️ Development Setup

1. **Prerequisites**:
   - Rust 1.85+ (Edition 2024 support)
   - Node.js & npm (for MCP testing, e.g. Playwright)

2. **Clone & Build**:
   ```bash
   git clone https://github.com/choratools/dustagent.git
   cd dustagent
   cargo build
   ```

3. **Run Tests**:
   ```bash
   cargo test
   ```

4. **Code Quality Gates**:
   Before submitting a PR, make sure all gates pass:
   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   ```

---

## 📐 Architecture Guidelines

- **Micro-Kernel Simplicity**: Keep the Rust core engine minimal. New agent capabilities should be added as **AaaA manifests in `apps/*.json`**, not by adding ad-hoc Rust logic.
- **Zero-Chatter Rule**: Prompts and outputs must adhere strictly to the zero-chatter policy. Do not add pleasantries, Markdown introductions, or conversational explanations to agent outputs.
- **Atomic Operations**: All file modification logic must guarantee 100% rollback on failure via `FuzzyPatcher`.

---

## 📄 License
By contributing to DustAgent, you agree that your contributions will be licensed under its dual MIT / Apache-2.0 license.
