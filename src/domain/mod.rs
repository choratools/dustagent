pub mod manifest;
pub mod patch;

pub use manifest::{AppManifest, McpChrootConfig, McpServerConfig, resolve_manifest_path};
pub use patch::{LineRange, PatchError, SearchReplaceBlock, extract_blocks};
