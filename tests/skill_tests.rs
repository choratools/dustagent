use dustagent::application::skills::SkillCatalog;
use std::{fs, path::Path};
fn setup(root: &Path) {
    let skill = root.join("skills/crawl");
    fs::create_dir_all(skill.join("references")).unwrap();
    fs::create_dir_all(skill.join("assets")).unwrap();
    fs::create_dir_all(skill.join("scripts")).unwrap();
    fs::write(skill.join("scripts/check.sh"), "exit 99").unwrap();
    fs::write(skill.join("SKILL.md"), "---\nname: crawl\ndescription: |\n  Read all pages\n  and check coverage.\nmetadata:\n  author: test\n---\nSecret body\n").unwrap();
    fs::write(skill.join("references/coverage.md"), "coverage").unwrap();
    fs::write(skill.join("assets/template.json"), "{}").unwrap();
}
fn load(root: &Path) -> SkillCatalog {
    SkillCatalog::load(root, &["crawl".into()]).unwrap()
}
#[test]
fn scoped_reads_and_metadata_only_summary() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path());
    let cat = load(root.path());
    assert!(cat.summary().contains("coverage"));
    assert!(!cat.summary().contains("Secret body"));
    assert!(cat.read("crawl", None).unwrap().contains("Secret body"));
    assert_eq!(
        cat.read("crawl", Some("references/coverage.md")).unwrap(),
        "coverage"
    );
    assert_eq!(
        cat.read("crawl", Some("assets/template.json")).unwrap(),
        "{}"
    );
    for path in [
        "../other/SKILL.md",
        "/etc/passwd",
        "references/../../other",
        "app.json",
    ] {
        assert!(cat.read("crawl", Some(path)).is_err());
    }
    assert!(cat.read("other", None).is_err());
    assert_eq!(
        cat.read("crawl", Some("scripts/check.sh")).unwrap(),
        "exit 99"
    );
}
#[test]
fn digest_relocation_and_changes() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    setup(a.path());
    setup(b.path());
    assert_eq!(load(a.path()).digest(), load(b.path()).digest());
    fs::write(
        b.path().join("skills/crawl/references/coverage.md"),
        "changed",
    )
    .unwrap();
    assert_ne!(load(a.path()).digest(), load(b.path()).digest());
}
#[test]
fn malformed_and_invalid_metadata_rejected() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path());
    for text in [
        "body",
        "---\nname: [\ndescription: text\n---",
        "---\nname: wrong\ndescription: text\n---",
        "---\nname: crawl\ndescription: ''\n---",
        "---\nname: crawl\ndescription: text\nmetadata:\n  bad: 123\n---",
    ] {
        fs::write(root.path().join("skills/crawl/SKILL.md"), text).unwrap();
        assert!(SkillCatalog::load(root.path(), &["crawl".into()]).is_err());
    }
    assert!(SkillCatalog::load(root.path(), &["../crawl".into()]).is_err());
}
#[test]
fn oversized_text_refused_without_truncation() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path());
    fs::write(
        root.path().join("skills/crawl/references/large.md"),
        vec![b'x'; 65537],
    )
    .unwrap();
    let cat = load(root.path());
    assert!(cat.read("crawl", Some("references/large.md")).is_err());
}
#[cfg(unix)]
#[test]
fn symlink_escape_refused_at_load_and_read() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    setup(root.path());
    let cat = load(root.path());
    symlink(
        "/etc/passwd",
        root.path().join("skills/crawl/references/outside"),
    )
    .unwrap();
    assert!(cat.read("crawl", Some("references/outside")).is_err());
    assert!(SkillCatalog::load(root.path(), &["crawl".into()]).is_err());
}

#[test]
fn duplicate_and_standard_bounds_rejected() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path());
    assert!(SkillCatalog::load(root.path(), &["crawl".into(), "crawl".into()]).is_err());
    assert!(SkillCatalog::load(root.path(), &["bad--name".into()]).is_err());
    for (key, value) in [
        ("description", "x".repeat(1025)),
        ("compatibility", "x".repeat(501)),
    ] {
        let text = if key == "description" {
            format!("---\nname: crawl\ndescription: {value}\n---")
        } else {
            format!("---\nname: crawl\ndescription: okay\ncompatibility: {value}\n---")
        };
        fs::write(root.path().join("skills/crawl/SKILL.md"), text).unwrap();
        assert!(SkillCatalog::load(root.path(), &["crawl".into()]).is_err());
    }
}
