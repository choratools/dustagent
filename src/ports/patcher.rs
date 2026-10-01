use std::path::Path;

use crate::domain::patch::SearchReplaceBlock;

/// Port defining the contract for code patch engines.
pub trait CodePatcher: Send + Sync {
    /// Applies a single search/replace block to the given original string.
    fn apply_patch(&self, original: &str, block: &SearchReplaceBlock) -> crate::Result<String>;

    /// Applies multiple search/replace blocks sequentially to the given original string.
    fn apply_blocks(&self, original: &str, blocks: &[SearchReplaceBlock]) -> crate::Result<String> {
        let mut current = original.to_string();
        for block in blocks {
            current = self.apply_patch(&current, block)?;
        }
        Ok(current)
    }

    /// Applies multiple search/replace blocks atomically to a file on disk.
    fn apply_to_file(&self, path: &Path, blocks: &[SearchReplaceBlock]) -> crate::Result<()>;
}
