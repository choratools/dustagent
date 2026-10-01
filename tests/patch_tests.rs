use std::fs;
use std::path::Path;

use dustagent::adapters::fuzzy_patch::FuzzyPatcher;
use dustagent::domain::patch::{LineRange, SearchReplaceBlock, extract_blocks};
use dustagent::ports::patcher::CodePatcher;
use tempfile::tempdir;

// ============================================================================
// 1. Exact Match Replacement Tests
// ============================================================================

#[test]
fn test_exact_match_single_line() {
    let patcher = FuzzyPatcher::new();
    let original = "fn main() {\n    println!(\"Hello, world!\");\n}\n";
    let block = SearchReplaceBlock::new(
        "    println!(\"Hello, world!\");",
        "    println!(\"Hello, DustAgent!\");",
    )
    .unwrap();

    let result = patcher.apply_patch(original, &block).unwrap();
    let expected = "fn main() {\n    println!(\"Hello, DustAgent!\");\n}\n";
    assert_eq!(result, expected);
}

#[test]
fn test_exact_match_multiline() {
    let patcher = FuzzyPatcher::new();
    let original = r#"fn calculate(x: i32) -> i32 {
    let a = x * 2;
    let b = a + 1;
    b
}
"#;
    let search = r#"    let a = x * 2;
    let b = a + 1;"#;
    let replace = r#"    let a = x * 4;
    let b = a + 2;"#;
    let block = SearchReplaceBlock::new(search, replace).unwrap();

    let result = patcher.apply_patch(original, &block).unwrap();
    let expected = r#"fn calculate(x: i32) -> i32 {
    let a = x * 4;
    let b = a + 2;
    b
}
"#;
    assert_eq!(result, expected);
}

#[test]
fn test_exact_match_at_start_and_end() {
    let patcher = FuzzyPatcher::new();
    let original = "HEADER\nbody content\nFOOTER";

    let block_start = SearchReplaceBlock::new("HEADER", "NEW_HEADER").unwrap();
    let res1 = patcher.apply_patch(original, &block_start).unwrap();
    assert_eq!(res1, "NEW_HEADER\nbody content\nFOOTER");

    let block_end = SearchReplaceBlock::new("FOOTER", "NEW_FOOTER").unwrap();
    let res2 = patcher.apply_patch(&res1, &block_end).unwrap();
    assert_eq!(res2, "NEW_HEADER\nbody content\nNEW_FOOTER");
}

#[test]
fn test_exact_match_deletion() {
    let patcher = FuzzyPatcher::new();
    let original = "line 1\nline 2 (to delete)\nline 3\n";
    let block = SearchReplaceBlock::new("line 2 (to delete)\n", "").unwrap();

    let result = patcher.apply_patch(original, &block).unwrap();
    assert_eq!(result, "line 1\nline 3\n");
}

#[test]
fn test_exact_match_crlf_preservation() {
    let patcher = FuzzyPatcher::new();
    let original = "line 1\r\nline 2\r\nline 3\r\n";
    let block = SearchReplaceBlock::new("line 2", "line 2 modified").unwrap();

    let result = patcher.apply_patch(original, &block).unwrap();
    assert_eq!(result, "line 1\r\nline 2 modified\r\nline 3\r\n");
}

// ============================================================================
// 2. Whitespace and Indentation Tolerant Replacement Tests
// ============================================================================

#[test]
fn test_whitespace_tolerance_positive_indent_shift() {
    let patcher = FuzzyPatcher::new();
    // In target file, the function is nested inside an impl block (4 spaces indent)
    let original = r#"impl Service {
    fn run(&self) {
        let x = 10;
        println!("{}", x);
    }
}
"#;

    // Search block provided by LLM has 0 spaces base indent
    let search = r#"fn run(&self) {
    let x = 10;
    println!("{}", x);
}"#;

    // Replace block also has 0 spaces base indent
    let replace = r#"fn run(&self) {
    let x = 20;
    println!("Updated: {}", x);
}"#;

    let block = SearchReplaceBlock::new(search, replace).unwrap();
    let result = patcher.apply_patch(original, &block).unwrap();

    let expected = r#"impl Service {
    fn run(&self) {
        let x = 20;
        println!("Updated: {}", x);
    }
}
"#;
    assert_eq!(result, expected);
}

#[test]
fn test_whitespace_tolerance_negative_indent_shift() {
    let patcher = FuzzyPatcher::new();
    // Target file has 0 indentation
    let original = "let a = 1;\nlet b = 2;\n";

    // Search block has 4 spaces indent
    let search = "    let a = 1;\n    let b = 2;";
    // Replace block has 4 spaces indent with sub-indentation
    let replace = "    let a = 10;\n        let b = 20;";

    let block = SearchReplaceBlock::new(search, replace).unwrap();
    let result = patcher.apply_patch(original, &block).unwrap();

    // Outdented by 4 spaces: "    let a = 10;" -> "let a = 10;", "        let b = 20;" -> "    let b = 20;"
    let expected = "let a = 10;\n    let b = 20;\n";
    assert_eq!(result, expected);
}

#[test]
fn test_whitespace_tolerance_trailing_spaces() {
    let patcher = FuzzyPatcher::new();
    // Target file has trailing spaces on lines
    let original = "let x = 1;   \nlet y = 2;\t\n";
    let block =
        SearchReplaceBlock::new("let x = 1;\nlet y = 2;", "let x = 100;\nlet y = 200;").unwrap();

    let result = patcher.apply_patch(original, &block).unwrap();
    assert_eq!(result, "let x = 100;\nlet y = 200;\n");
}

// ============================================================================
// 3. Levenshtein / Similarity Sliding Window Fuzzy Replacement Tests
// ============================================================================

#[test]
fn test_fuzzy_levenshtein_single_line_typo() {
    let patcher = FuzzyPatcher::new();
    // Target file has a slight typo or variation
    let original = r#"pub fn process_data(items: &[String]) -> usize {
    let mut item_countr = 0;
    item_countr += items.len();
    item_countr
}
"#;

    // Search block contains corrected spelling
    let search = r#"    let mut item_counter = 0;
    item_counter += items.len();
    item_counter"#;

    let replace = r#"    let mut total = 0;
    total += items.len();
    total"#;

    let block = SearchReplaceBlock::new(search, replace).unwrap();
    let result = patcher.apply_patch(original, &block).unwrap();

    let expected = r#"pub fn process_data(items: &[String]) -> usize {
    let mut total = 0;
    total += items.len();
    total
}
"#;
    assert_eq!(result, expected);
}

#[test]
fn test_fuzzy_matching_fails_on_low_similarity() {
    let patcher = FuzzyPatcher::new();
    let original = "alpha beta gamma delta";
    let block = SearchReplaceBlock::new("completely different text", "replacement").unwrap();

    let err = patcher.apply_patch(original, &block).unwrap_err();
    assert!(err.to_string().contains("Failed to match search block"));
}

#[test]
fn test_fuzzy_custom_similarity_threshold() {
    // When threshold is very high (0.99), minor typo won't match
    let strict_patcher = FuzzyPatcher::with_threshold(0.99);
    let original = "let calculate_total_amount = 42;\n";
    // Missing an 'l' in calculate -> similarity ~ 0.96
    let block =
        SearchReplaceBlock::new("let caculate_total_amount = 42;", "let total = 100;").unwrap();

    assert!(strict_patcher.apply_patch(original, &block).is_err());

    // With default threshold (0.85), it should match
    let default_patcher = FuzzyPatcher::new();
    let res = default_patcher.apply_patch(original, &block).unwrap();
    assert_eq!(res, "let total = 100;\n");
}

// ============================================================================
// 4. Multiple Search/Replace Blocks in Sequence Tests
// ============================================================================

#[test]
fn test_multiple_blocks_sequential() {
    let patcher = FuzzyPatcher::new();
    let original = r#"// Version: 1.0.0
fn get_user() -> String {
    "alice".to_string()
}

fn get_role() -> String {
    "guest".to_string()
}
"#;

    let b1 = SearchReplaceBlock::new("// Version: 1.0.0", "// Version: 2.0.0").unwrap();
    let b2 = SearchReplaceBlock::new(
        "fn get_user() -> String {\n    \"alice\".to_string()\n}",
        "fn get_user() -> String {\n    \"bob\".to_string()\n}",
    )
    .unwrap();
    let b3 = SearchReplaceBlock::new(
        "fn get_role() -> String {\n    \"guest\".to_string()\n}",
        "fn get_role() -> String {\n    \"admin\".to_string()\n}",
    )
    .unwrap();

    let blocks = vec![b1, b2, b3];
    let result = patcher.apply_blocks(original, &blocks).unwrap();

    let expected = r#"// Version: 2.0.0
fn get_user() -> String {
    "bob".to_string()
}

fn get_role() -> String {
    "admin".to_string()
}
"#;
    assert_eq!(result, expected);
}

// ============================================================================
// 5. Extracting Blocks from LLM Markdown Code Blocks Tests
// ============================================================================

#[test]
fn test_extract_blocks_from_llm_markdown() {
    let llm_output = r#"I'll update the math logic and logging.

```rust
<<<<<<< SEARCH
fn add(a: i32, b: i32) -> i32 {
    a + b
}
=======
fn add(a: i32, b: i32) -> i32 {
    // Optimized addition
    a.saturating_add(b)
}
>>>>>>> REPLACE
```

And update the log level:

```rust
<<<<<<< SEARCH
let level = "debug";
=======
let level = "info";
>>>>>>> REPLACE
```

Done!
"#;

    let blocks = extract_blocks(llm_output);
    assert_eq!(blocks.len(), 2);

    assert_eq!(
        blocks[0].search(),
        "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}"
    );
    assert_eq!(
        blocks[0].replace(),
        "fn add(a: i32, b: i32) -> i32 {\n    // Optimized addition\n    a.saturating_add(b)\n}"
    );

    assert_eq!(blocks[1].search(), "let level = \"debug\";");
    assert_eq!(blocks[1].replace(), "let level = \"info\";");
}

#[test]
fn test_extract_blocks_crlf_and_deletion() {
    let llm_output = "<<<<<<< SEARCH\r\nold_code();\r\n=======\r\n>>>>>>> REPLACE\r\n";
    let blocks = extract_blocks(llm_output);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].search(), "old_code();");
    assert_eq!(blocks[0].replace(), "");
}

// ============================================================================
// 6. Atomic File Patching Tests
// ============================================================================

#[test]
fn test_apply_to_file_atomic_success() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("main.rs");
    let original = "fn main() {\n    println!(\"old\");\n}\n";
    fs::write(&file_path, original).unwrap();

    let patcher = FuzzyPatcher::new();
    let block = SearchReplaceBlock::new("println!(\"old\");", "println!(\"new\");").unwrap();

    patcher.apply_to_file(&file_path, &[block]).unwrap();

    let updated = fs::read_to_string(&file_path).unwrap();
    assert_eq!(updated, "fn main() {\n    println!(\"new\");\n}\n");

    // Ensure temp file was cleaned up
    let temp_file = dir.path().join("main.rs.dust.tmp");
    assert!(!temp_file.exists());
}

#[test]
fn test_apply_to_file_non_existent_returns_err() {
    let patcher = FuzzyPatcher::new();
    let block = SearchReplaceBlock::new("a", "b").unwrap();
    let non_existent = Path::new("/tmp/dustagent_non_existent_file_12345.rs");

    let err = patcher.apply_to_file(non_existent, &[block]).unwrap_err();
    assert!(err.to_string().contains("Target file not found"));
}

#[test]
fn test_apply_to_file_rollback_on_failure() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("config.toml");
    let original = "port = 8080\nhost = \"localhost\"\n";
    fs::write(&file_path, original).unwrap();

    let patcher = FuzzyPatcher::new();
    // First block valid, second block invalid/unmatchable
    let b1 = SearchReplaceBlock::new("port = 8080", "port = 9090").unwrap();
    let b2 = SearchReplaceBlock::new("database_url = ...", "database_url = new").unwrap();

    let result = patcher.apply_to_file(&file_path, &[b1, b2]);
    assert!(result.is_err());

    // File on disk must remain UNTOUCHED because failure occurred before atomic rename
    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, original);

    // Temp file must be cleaned up
    let temp_file = dir.path().join("config.toml.dust.tmp");
    assert!(!temp_file.exists());
}

// ============================================================================
// 7. LineRange Tests
// ============================================================================

#[test]
fn test_line_range_parsing() {
    let r1 = LineRange::parse("10:25").unwrap();
    assert_eq!(r1.start(), 10);
    assert_eq!(r1.end(), 25);
    assert_eq!(r1.len(), 16);
    assert!(r1.contains(10));
    assert!(r1.contains(25));
    assert!(!r1.contains(9));
    assert!(!r1.contains(26));
    assert_eq!(r1.to_slice_range(), 9..25);
    assert_eq!(r1.to_string(), "10:25");

    let r2 = LineRange::parse("42").unwrap();
    assert_eq!(r2.start(), 42);
    assert_eq!(r2.end(), 42);
    assert_eq!(r2.len(), 1);
    assert_eq!(r2.to_string(), "42");
    assert_eq!(r2.to_slice_range(), 41..42);

    let r3 = LineRange::parse("  5 : 12  ").unwrap();
    assert_eq!(r3.start(), 5);
    assert_eq!(r3.end(), 12);
}

#[test]
fn test_line_range_invariants_and_errors() {
    assert!(LineRange::new(0, 10).is_err());
    assert!(LineRange::new(10, 5).is_err());
    assert!(LineRange::parse("").is_err());
    assert!(LineRange::parse("0").is_err());
    assert!(LineRange::parse("abc").is_err());
    assert!(LineRange::parse("10:abc").is_err());
    assert!(LineRange::parse("20:10").is_err());
}

// ============================================================================
// 8. SearchReplaceBlock Invariant Tests
// ============================================================================

#[test]
fn test_search_replace_block_invariants() {
    assert!(SearchReplaceBlock::new("", "replacement").is_err());
    assert!(SearchReplaceBlock::new("   \n\t  ", "replacement").is_err());
    assert!(SearchReplaceBlock::new("valid search", "replacement").is_ok());
    assert!(SearchReplaceBlock::new("valid search", "").is_ok());
}
