#!/usr/bin/env python3
"""
Dust: Minimalist Agent as an Application (AaaA) CLI.
Zero-Dependency, Pure I/O, Scoped MCP.
"""

import argparse
import os
import sys

# Ensure local dustagent package is in PYTHONPATH
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from dustagent.core import DustCore
from dustagent.patch import FuzzyPatcher


def get_app_manifest_path(app_name: str) -> str:
    base_dir = os.path.dirname(os.path.abspath(__file__))
    candidates = [
        os.path.join(base_dir, "apps", f"{app_name}.json"),
        os.path.join(base_dir, "apps", f"{app_name}.yaml"),
        app_name  # direct path
    ]
    for c in candidates:
        if os.path.exists(c):
            return c
    raise FileNotFoundError(f"App manifest '{app_name}' not found in apps/ directory.")


def cmd_run(args):
    manifest_path = get_app_manifest_path(args.app)
    
    # Read user input from argument or STDIN
    if args.input:
        user_input = " ".join(args.input)
    elif not sys.stdin.isatty():
        user_input = sys.stdin.read().strip()
    else:
        sys.stderr.write("Error: No input provided via argument or STDIN.\n")
        sys.exit(1)

    with DustCore(manifest_path=manifest_path, model=args.model) as core:
        output = core.execute(user_input)
        sys.stdout.write(output + "\n")
        sys.stdout.flush()


def cmd_patch(args):
    manifest_path = get_app_manifest_path("patcher")
    
    if not os.path.exists(args.file):
        sys.stderr.write(f"Error: File not found: {args.file}\n")
        sys.exit(1)

    with open(args.file, "r", encoding="utf-8") as f:
        file_content = f.read()

    # High-density surrounding context assembly
    lines = file_content.splitlines()
    if args.range:
        parts = args.range.split(":")
        start_line = max(1, int(parts[0]))
        end_line = int(parts[1]) if len(parts) > 1 else start_line
        
        # Surrounding context window (+- 30 lines)
        win_start = max(1, start_line - 30)
        win_end = min(len(lines), end_line + 30)

        context_snippet = "\n".join(
            f"{i+1:4d} | {lines[i]}" for i in range(win_start - 1, win_end)
        )
        target_snippet = "\n".join(lines[start_line - 1:end_line])
        user_prompt = (
            f"Target file: {args.file} (Lines {start_line}-{end_line})\n"
            f"Surrounding context:\n```\n{context_snippet}\n```\n\n"
            f"Target snippet to edit:\n```\n{target_snippet}\n```\n\n"
            f"Instruction: {args.instruction}"
        )
    else:
        user_prompt = f"File: {args.file}\n\n```\n{file_content}\n```\n\nInstruction: {args.instruction}"

    with DustCore(manifest_path=manifest_path, model=args.model) as core:
        result = core.execute(user_prompt)
        blocks = FuzzyPatcher.extract_blocks(result)

        if not blocks:
            sys.stderr.write("Error: Model did not produce valid SEARCH/REPLACE blocks.\n")
            sys.stderr.write(f"Model output was:\n{result}\n")
            sys.exit(3)

        if args.dry_run:
            sys.stdout.write(f"[Dry Run] Generated {len(blocks)} patch block(s):\n")
            sys.stdout.write(result + "\n")
        else:
            FuzzyPatcher.apply_blocks_to_file(args.file, blocks)
            sys.stderr.write(f"Successfully applied {len(blocks)} patch block(s) to {args.file}\n")


def main():
    parser = argparse.ArgumentParser(
        prog="dust",
        description="DustAgent: Ultra-lightweight Agent as an Application CLI"
    )
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    # 'run' subcommand
    run_parser = subparsers.add_parser("run", help="Run a specialized application manifest")
    run_parser.add_argument("app", help="Application name in apps/ (e.g. crawler, sql)")
    run_parser.add_argument("input", nargs="*", help="Input query, URL, or prompt")
    run_parser.add_argument("-m", "--model", help="Override default LLM model")
    run_parser.set_defaults(func=cmd_run)

    # 'patch' subcommand
    patch_parser = subparsers.add_parser("patch", help="Direct in-place code patcher")
    patch_parser.add_argument("-f", "--file", required=True, help="Target file path to edit")
    patch_parser.add_argument("-r", "--range", help="Line range to edit (e.g. 15:30)")
    patch_parser.add_argument("-m", "--model", help="Override default LLM model")
    patch_parser.add_argument("--dry-run", action="store_true", help="Print diff blocks without modifying file")
    patch_parser.add_argument("instruction", help="Edit instruction for the code")
    patch_parser.set_defaults(func=cmd_patch)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
