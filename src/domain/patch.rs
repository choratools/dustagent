use std::fmt;
use std::ops::Range;
use std::str::FromStr;
use std::sync::LazyLock;

use regex::Regex;

static SEARCH_REPLACE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    regex::RegexBuilder::new(
        r"<<<<<<<\s*SEARCH[^\S\r\n]*\r?\n(.*?)\r?\n=======[^\S\r\n]*(?:\r?\n(.*?))?\r?\n>>>>>>>\s*REPLACE",
    )
    .dot_matches_new_line(true)
    .build()
    .expect("Valid search/replace block regex")
});

/// Specific patch-related error conditions.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PatchError {
    #[error("Search block cannot be empty or whitespace")]
    EmptySearchBlock,

    #[error("Failed to match search block in target content even with fuzzy matching")]
    MatchNotFound,

    #[error("Invalid line range: {0}")]
    InvalidLineRange(String),
}

impl From<PatchError> for crate::DustError {
    fn from(err: PatchError) -> Self {
        crate::DustError::Patch(err.to_string())
    }
}

/// A single search and replace block following the Aider search/replace protocol.
///
/// Invariant: `search` must not be empty or purely whitespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchReplaceBlock {
    search: String,
    replace: String,
}

impl SearchReplaceBlock {
    /// Creates a new `SearchReplaceBlock` validating that the search block is not empty or whitespace.
    pub fn new(search: impl Into<String>, replace: impl Into<String>) -> crate::Result<Self> {
        let search = search.into();
        if search.trim().is_empty() {
            return Err(PatchError::EmptySearchBlock.into());
        }
        Ok(Self {
            search,
            replace: replace.into(),
        })
    }

    /// Returns a reference to the search string.
    pub fn search(&self) -> &str {
        &self.search
    }

    /// Returns a reference to the replace string.
    pub fn replace(&self) -> &str {
        &self.replace
    }

    /// Consumes the block and returns the owned `(search, replace)` pair.
    pub fn into_parts(self) -> (String, String) {
        (self.search, self.replace)
    }

    /// Extracts all search/replace blocks from the given text.
    pub fn extract_blocks(text: &str) -> Vec<Self> {
        extract_blocks(text)
    }
}

/// Extracts all search/replace blocks from text matching:
/// `<<<<<<<\s*SEARCH\r?\n(.*?)\r?\n=======\r?\n(.*?)\r?\n>>>>>>>\s*REPLACE`
pub fn extract_blocks(text: &str) -> Vec<SearchReplaceBlock> {
    SEARCH_REPLACE_REGEX
        .captures_iter(text)
        .filter_map(|cap| {
            let search = cap.get(1).map(|m| m.as_str())?;
            let replace = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            SearchReplaceBlock::new(search, replace).ok()
        })
        .collect()
}

/// Represents a 1-based inclusive line range (e.g. "10:20" or "15").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl LineRange {
    /// Creates a new 1-based inclusive `LineRange`.
    ///
    /// # Errors
    /// Returns an error if `start == 0` or if `start > end`.
    pub fn new(start: usize, end: usize) -> crate::Result<Self> {
        if start == 0 {
            return Err(PatchError::InvalidLineRange(
                "Line numbers are 1-based; line 0 is invalid".into(),
            )
            .into());
        }
        if start > end {
            return Err(PatchError::InvalidLineRange(format!(
                "Start line ({start}) cannot be greater than end line ({end})"
            ))
            .into());
        }
        Ok(Self { start, end })
    }

    /// Parses a line range string in "start:end" or "start" format.
    pub fn parse(s: &str) -> crate::Result<Self> {
        s.parse()
    }

    /// Returns the 1-based start line number.
    pub fn start(&self) -> usize {
        self.start
    }

    /// Returns the 1-based end line number.
    pub fn end(&self) -> usize {
        self.end
    }

    /// Returns the number of lines covered by this range.
    pub fn len(&self) -> usize {
        self.end - self.start + 1
    }

    /// Line range always covers at least one line (since start <= end).
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Checks if a 1-based line number falls within this range.
    pub fn contains(&self, line: usize) -> bool {
        line >= self.start && line <= self.end
    }

    /// Converts the 1-based inclusive range to a 0-based half-open range `[start-1, end)`
    /// suitable for slicing line arrays.
    pub fn to_slice_range(&self) -> Range<usize> {
        (self.start - 1)..self.end
    }
}

impl FromStr for LineRange {
    type Err = crate::DustError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(PatchError::InvalidLineRange("Empty line range string".into()).into());
        }

        if let Some((start_str, end_str)) = s.split_once(':') {
            let start: usize = start_str.trim().parse().map_err(|_| {
                PatchError::InvalidLineRange(format!(
                    "Invalid start line number in range: '{start_str}'"
                ))
            })?;
            let end: usize = end_str.trim().parse().map_err(|_| {
                PatchError::InvalidLineRange(format!(
                    "Invalid end line number in range: '{end_str}'"
                ))
            })?;
            Self::new(start, end)
        } else {
            let line: usize = s
                .parse()
                .map_err(|_| PatchError::InvalidLineRange(format!("Invalid line number: '{s}'")))?;
            Self::new(line, line)
        }
    }
}

impl fmt::Display for LineRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}:{}", self.start, self.end)
        }
    }
}
