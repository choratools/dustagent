use std::io::Write;
use std::path::Path;

use crate::domain::patch::SearchReplaceBlock;
use crate::ports::patcher::CodePatcher;

/// Robust 4-stage search/replace patch engine inspired by Aider and dustagent/patch.py.
///
/// Multi-stage matching strategy:
/// 1. Exact string match (`original.find(block.search())`)
/// 2. Whitespace-stripped line matching
/// 3. Relative indentation shift adjustment
/// 4. Levenshtein / Jaro-Winkler similarity sliding window fallback
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyPatcher {
    similarity_threshold: f64,
}

impl Default for FuzzyPatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl FuzzyPatcher {
    pub const DEFAULT_SIMILARITY_THRESHOLD: f64 = 0.85;

    /// Creates a new `FuzzyPatcher` with the default similarity threshold (0.85).
    pub fn new() -> Self {
        Self {
            similarity_threshold: Self::DEFAULT_SIMILARITY_THRESHOLD,
        }
    }

    /// Creates a new `FuzzyPatcher` with a custom similarity threshold.
    pub fn with_threshold(similarity_threshold: f64) -> Self {
        Self {
            similarity_threshold,
        }
    }

    /// Returns the configured similarity threshold.
    pub fn similarity_threshold(&self) -> f64 {
        self.similarity_threshold
    }

    /// Applies a single patch string directly (compatible with Python `apply_patch_to_string`).
    pub fn apply_patch_to_string(
        original_text: &str,
        search_block: &str,
        replace_block: &str,
    ) -> crate::Result<String> {
        let block = SearchReplaceBlock::new(search_block, replace_block)?;
        let patcher = Self::new();
        patcher.apply_patch(original_text, &block)
    }

    /// Extracts search/replace blocks from text.
    pub fn extract_blocks(text: &str) -> Vec<SearchReplaceBlock> {
        crate::domain::patch::extract_blocks(text)
    }

    /// Applies blocks to a file atomically (compatible with Python `apply_blocks_to_file`).
    pub fn apply_blocks_to_file(
        path: impl AsRef<Path>,
        blocks: &[SearchReplaceBlock],
    ) -> crate::Result<()> {
        let patcher = Self::new();
        patcher.apply_to_file(path.as_ref(), blocks)
    }

    /// Internal helper that shifts indentation and reconstructs the output string.
    fn reconstruct_lines(
        orig_lines: &[&str],
        match_idx: usize,
        search_len: usize,
        search_lines: &[&str],
        replace_lines: &[&str],
        original: &str,
    ) -> String {
        // Stage 3: Relative Indentation Shift
        let (target_indent_len, search_indent_len) = {
            let mut found = None;
            for k in 0..search_len {
                if !search_lines[k].trim().is_empty() {
                    let s_indent = search_lines[k].len() - search_lines[k].trim_start().len();
                    let t_indent = orig_lines[match_idx + k].len()
                        - orig_lines[match_idx + k].trim_start().len();
                    found = Some((t_indent, s_indent));
                    break;
                }
            }
            found.unwrap_or_else(|| {
                let s_indent = search_lines[0].len() - search_lines[0].trim_start().len();
                let t_indent =
                    orig_lines[match_idx].len() - orig_lines[match_idx].trim_start().len();
                (t_indent, s_indent)
            })
        };

        let indent_delta = target_indent_len as isize - search_indent_len as isize;

        let mut adjusted_replace = Vec::with_capacity(replace_lines.len());
        for &r_line in replace_lines {
            if r_line.trim().is_empty() {
                adjusted_replace.push(String::new());
            } else if indent_delta > 0 {
                let indent = " ".repeat(indent_delta as usize);
                adjusted_replace.push(format!("{}{}", indent, r_line));
            } else if indent_delta < 0 {
                let cur_indent = r_line.len() - r_line.trim_start().len();
                let shift = ((-indent_delta) as usize).min(cur_indent);
                adjusted_replace.push(r_line[shift..].to_string());
            } else {
                adjusted_replace.push(r_line.to_string());
            }
        }

        let mut new_lines = Vec::with_capacity(orig_lines.len() + adjusted_replace.len());
        new_lines.extend(orig_lines[..match_idx].iter().map(|s| s.to_string()));
        new_lines.extend(adjusted_replace);
        new_lines.extend(
            orig_lines[match_idx + search_len..]
                .iter()
                .map(|s| s.to_string()),
        );

        let line_ending = if original.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut result = new_lines.join(line_ending);
        if original.ends_with('\n') && !result.is_empty() && !result.ends_with('\n') {
            result.push_str(line_ending);
        }
        result
    }
}

impl CodePatcher for FuzzyPatcher {
    fn apply_patch(&self, original: &str, block: &SearchReplaceBlock) -> crate::Result<String> {
        // Stage 1: Exact String Match
        if let Some(pos) = original.find(block.search()) {
            let mut result = String::with_capacity(original.len() + block.replace().len());
            result.push_str(&original[..pos]);
            result.push_str(block.replace());
            result.push_str(&original[pos + block.search().len()..]);
            return Ok(result);
        }

        // Split into lines for whitespace-tolerant matching
        let orig_lines: Vec<&str> = original.lines().collect();
        let search_lines: Vec<&str> = block.search().lines().collect();
        let replace_lines: Vec<&str> = if block.replace().is_empty() {
            Vec::new()
        } else {
            block.replace().lines().collect()
        };

        if search_lines.is_empty() {
            return Err(crate::DustError::Patch(
                "Empty search block provided".into(),
            ));
        }

        let search_stripped: Vec<&str> = search_lines.iter().map(|l| l.trim()).collect();
        let search_len = search_lines.len();
        let orig_len = orig_lines.len();

        // Stage 2: Whitespace-Stripped Line Matching
        if orig_len >= search_len {
            for i in 0..=(orig_len - search_len) {
                let window_matches = orig_lines[i..i + search_len]
                    .iter()
                    .zip(&search_stripped)
                    .all(|(orig_line, search_line)| orig_line.trim() == *search_line);

                if window_matches {
                    return Ok(Self::reconstruct_lines(
                        &orig_lines,
                        i,
                        search_len,
                        &search_lines,
                        &replace_lines,
                        original,
                    ));
                }
            }
        }

        // Stage 4: Levenshtein / Similarity Sliding Window Fallback
        let search_full = search_stripped.join("\n");
        let mut best_ratio = 0.0;
        let mut best_idx = None;

        if orig_len >= search_len {
            for i in 0..=(orig_len - search_len) {
                let window_stripped: Vec<&str> = orig_lines[i..i + search_len]
                    .iter()
                    .map(|l| l.trim())
                    .collect();
                let window_full = window_stripped.join("\n");

                let lev_score = strsim::normalized_levenshtein(&window_full, &search_full);
                let jw_score = strsim::jaro_winkler(&window_full, &search_full);
                let ratio = lev_score.max(jw_score);

                if ratio > best_ratio {
                    best_ratio = ratio;
                    best_idx = Some(i);
                }
            }
        }

        if best_ratio >= self.similarity_threshold
            && let Some(best_i) = best_idx
        {
            return Ok(Self::reconstruct_lines(
                &orig_lines,
                best_i,
                search_len,
                &search_lines,
                &replace_lines,
                original,
            ));
        }

        Err(crate::DustError::Patch(
            "Failed to match search block in target content even with fuzzy matching.".into(),
        ))
    }

    fn apply_to_file(&self, path: &Path, blocks: &[SearchReplaceBlock]) -> crate::Result<()> {
        if !path.exists() {
            return Err(crate::DustError::Patch(format!(
                "Target file not found: {}",
                path.display()
            )));
        }

        let content = std::fs::read_to_string(path)?;
        let mut current_content = content;
        for block in blocks {
            current_content = self.apply_patch(&current_content, block)?;
        }

        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "dust_file".to_string());
        let temp_path = parent.join(format!("{}.dust.tmp", file_name));

        let write_res = (|| -> Result<(), crate::DustError> {
            let mut file = std::fs::File::create(&temp_path)?;
            file.write_all(current_content.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp_path, path)?;
            Ok(())
        })();

        if write_res.is_err() {
            let _ = std::fs::remove_file(&temp_path);
        }

        write_res
    }
}
