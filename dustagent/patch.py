"""
Inline Fuzzy Search/Replace Patch Engine.
Implements multi-pass whitespace normalization and Levenshtein sliding window fallback.
"""

import difflib
import os
import re
from typing import List, Tuple


class FuzzyPatcher:
    """Robust in-place file patcher inspired by Aider's search/replace protocol."""

    SEARCH_REPLACE_PATTERN = re.compile(
        r"<<<<<<<\s*SEARCH\r?\n(.*?)\r?\n=======\r?\n(.*?)\r?\n>>>>>>>\s*REPLACE",
        re.DOTALL
    )

    @classmethod
    def extract_blocks(cls, text: str) -> List[Tuple[str, str]]:
        """Extracts all (search, replace) pairs from the model's output."""
        return cls.SEARCH_REPLACE_PATTERN.findall(text)

    @classmethod
    def apply_patch_to_string(cls, original_text: str, search_block: str, replace_block: str) -> str:
        """Applies a single search/replace block using a 4-stage matching strategy."""
        
        # Stage 1: Exact String Match
        if search_block in original_text:
            return original_text.replace(search_block, replace_block, 1)

        # Split into lines for whitespace-tolerant matching
        orig_lines = original_text.splitlines()
        search_lines = search_block.splitlines()
        replace_lines = replace_block.splitlines()

        if not search_lines:
            raise ValueError("Empty search block provided")

        # Stage 2: Whitespace-Stripped Line Matching
        search_stripped = [l.strip() for l in search_lines]
        search_len = len(search_lines)

        for i in range(len(orig_lines) - search_len + 1):
            window = [l.strip() for l in orig_lines[i:i + search_len]]
            if window == search_stripped:
                # Stage 3: Relative Indentation Shift
                target_indent_len = len(orig_lines[i]) - len(orig_lines[i].lstrip())
                search_indent_len = len(search_lines[0]) - len(search_lines[0].lstrip())
                indent_delta = target_indent_len - search_indent_len

                adjusted_replace = []
                for r_line in replace_lines:
                    if indent_delta > 0:
                        adjusted_replace.append((" " * indent_delta) + r_line)
                    elif indent_delta < 0:
                        shift = min(-indent_delta, len(r_line) - len(r_line.lstrip()))
                        adjusted_replace.append(r_line[shift:])
                    else:
                        adjusted_replace.append(r_line)

                new_lines = orig_lines[:i] + adjusted_replace + orig_lines[i + search_len:]
                return "\n".join(new_lines)

        # Stage 4: Levenshtein Sliding Window Fallback
        best_ratio = 0.0
        best_idx = -1
        search_full = "\n".join(search_stripped)

        for i in range(len(orig_lines) - search_len + 1):
            window_full = "\n".join(l.strip() for l in orig_lines[i:i + search_len])
            ratio = difflib.SequenceMatcher(None, window_full, search_full).ratio()
            if ratio > best_ratio:
                best_ratio = ratio
                best_idx = i

        if best_ratio >= 0.85 and best_idx != -1:
            target_indent_len = len(orig_lines[best_idx]) - len(orig_lines[best_idx].lstrip())
            search_indent_len = len(search_lines[0]) - len(search_lines[0].lstrip())
            indent_delta = target_indent_len - search_indent_len

            adjusted_replace = []
            for r_line in replace_lines:
                if indent_delta > 0:
                    adjusted_replace.append((" " * indent_delta) + r_line)
                elif indent_delta < 0:
                    shift = min(-indent_delta, len(r_line) - len(r_line.lstrip()))
                    adjusted_replace.append(r_line[shift:])
                else:
                    adjusted_replace.append(r_line)

            new_lines = orig_lines[:best_idx] + adjusted_replace + orig_lines[best_idx + search_len:]
            return "\n".join(new_lines)

        raise ValueError("Failed to match search block in target content even with fuzzy matching.")

    @classmethod
    def apply_blocks_to_file(cls, filepath: str, blocks: List[Tuple[str, str]]) -> None:
        """Applies multiple search/replace blocks atomically to a file."""
        if not os.path.exists(filepath):
            raise FileNotFoundError(f"Target file not found: {filepath}")

        with open(filepath, "r", encoding="utf-8") as f:
            content = f.read()

        current_content = content
        for search_block, replace_block in blocks:
            current_content = cls.apply_patch_to_string(current_content, search_block, replace_block)

        # Atomic replacement using temp file
        temp_file = f"{filepath}.dust.tmp"
        with open(temp_file, "w", encoding="utf-8") as f:
            f.write(current_content)
            f.flush()
            os.fsync(f.fileno())

        os.replace(temp_file, filepath)
