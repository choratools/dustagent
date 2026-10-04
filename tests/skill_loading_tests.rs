use dustagent::application::skills::{
    ModelSkillConfig, SkillCatalog, SkillInclude, SkillLoadingMode,
};
use serde_json::{Value, json};
use std::fs;

fn fixture() -> (tempfile::TempDir, SkillCatalog) {
    let root = tempfile::tempdir().unwrap();
    for name in ["review", "report"] {
        let dir = root.path().join("skills").join(name);
        fs::create_dir_all(dir.join("references")).unwrap();
        fs::create_dir_all(dir.join("scripts")).unwrap();
        fs::write(dir.join("SKILL.md"), format!("---\nname: {name}\ndescription: Describe {name}\n---\nPrivate body {name}. See references/check.md")).unwrap();
        fs::write(
            dir.join("references/check.md"),
            format!("Reference body {name}"),
        )
        .unwrap();
        fs::write(dir.join("scripts/check.sh"), "exit 99").unwrap();
    }
    let catalog = SkillCatalog::load(root.path(), &["review".into()]).unwrap();
    (root, catalog)
}
fn config(mode: SkillLoadingMode) -> ModelSkillConfig {
    ModelSkillConfig {
        mode,
        ..Default::default()
    }
}
fn selected(skill: &str, path: &str) -> ModelSkillConfig {
    ModelSkillConfig {
        mode: SkillLoadingMode::Selective,
        include: vec![SkillInclude {
            skill: skill.into(),
            paths: vec![path.into()],
        }],
        ..Default::default()
    }
}
#[test]
fn catalog_stays_small_and_default_compatible() {
    let (_, catalog) = fixture();
    let config = ModelSkillConfig {
        max_preload_bytes: 1,
        ..Default::default()
    };
    let prompt = catalog.prompt(&config).unwrap();
    let parsed: Value = serde_json::from_str(&prompt).unwrap();
    assert_eq!(parsed["catalog"][0]["name"], "review");
    assert!(parsed.get("resources").is_none());
    assert!(!prompt.contains("Private body"));
    assert!(!prompt.contains("check.md"));
    assert!(prompt.contains("dustagent__read_skill"));
    assert_eq!(
        serde_json::from_value::<ModelSkillConfig>(json!({})).unwrap(),
        ModelSkillConfig::default()
    );
}
#[test]
fn preload_only_skill_body_with_relative_resource_inventory() {
    let (_root, catalog) = fixture();
    let prompt = catalog.prompt(&config(SkillLoadingMode::Preload)).unwrap();
    let parsed: Value = serde_json::from_str(&prompt).unwrap();
    assert_eq!(parsed["preloaded"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["preloaded"][0]["skill"], "review");
    assert_eq!(parsed["preloaded"][0]["path"], "SKILL.md");
    assert!(prompt.contains("Private body review"));
    assert!(!prompt.contains("Reference body"));
    assert!(!prompt.contains("exit 99"));
    assert!(!prompt.contains("Private body report"));
    let inventory = parsed["resources"].as_array().unwrap();
    assert_eq!(inventory.len(), 3);
    assert_eq!(
        inventory[0],
        json!({"skill":"review","path":"SKILL.md","preloaded":true})
    );
    assert_eq!(inventory[1]["path"], "references/check.md");
    assert_eq!(inventory[1]["preloaded"], false);
    assert_eq!(
        prompt,
        catalog.prompt(&config(SkillLoadingMode::Preload)).unwrap()
    );
}
#[test]
fn selective_loads_exact_files_without_executing_scripts() {
    let (_root, catalog) = fixture();
    let mut config = selected("review", "references/check.md");
    config.include[0].paths.push("scripts/check.sh".into());
    let prompt = catalog.prompt(&config).unwrap();
    assert!(prompt.contains("Reference body review"));
    assert!(prompt.contains("exit 99"));
    assert!(!prompt.contains("Private body"));
    let parsed: Value = serde_json::from_str(&prompt).unwrap();
    assert_eq!(parsed["preloaded"].as_array().unwrap().len(), 2);
    assert_eq!(parsed["resources"][0]["preloaded"], false);
}
#[test]
fn selection_cannot_expand_scope_or_escape_and_missing_is_error() {
    let (_root, catalog) = fixture();
    for (skill, path) in [
        ("report", "SKILL.md"),
        ("review", "../report/SKILL.md"),
        ("review", "/etc/passwd"),
        ("review", "references/missing.md"),
        ("review", "references"),
    ] {
        assert!(
            catalog.prompt(&selected(skill, path)).is_err(),
            "{skill}/{path}"
        );
    }
}
#[test]
fn preload_budget_covers_whole_serialized_payload_and_never_truncates() {
    let (_root, catalog) = fixture();
    let mut config = config(SkillLoadingMode::Preload);
    let payload = catalog.prompt(&config).unwrap();
    config.max_preload_bytes = payload.len();
    assert_eq!(catalog.prompt(&config).unwrap(), payload);
    config.max_preload_bytes -= 1;
    assert!(
        catalog
            .prompt(&config)
            .unwrap_err()
            .to_string()
            .contains("max_preload_bytes")
    );
}
#[test]
fn invalid_configuration_and_unknown_fields_rejected() {
    let mut config = selected("review", "SKILL.md");
    config.include[0].paths.push("SKILL.md".into());
    assert!(config.validate().is_err());
    config.include[0].paths.pop();
    config.include.push(config.include[0].clone());
    assert!(config.validate().is_err());
    config.include.pop();
    config.mode = SkillLoadingMode::Catalog;
    assert!(config.validate().is_err());
    config.include.clear();
    config.mode = SkillLoadingMode::Selective;
    assert!(config.validate().is_err());
    for budget in [0, 262145] {
        let config = ModelSkillConfig {
            max_preload_bytes: budget,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
    for value in [
        json!({"unknown":true}),
        json!({"mode":"other"}),
        json!({"include":[{"skill":"review","paths":["SKILL.md"],"unknown":true}]}),
    ] {
        assert!(serde_json::from_value::<ModelSkillConfig>(value).is_err());
    }
}
#[test]
fn invalid_utf8_is_rejected_only_if_preloaded() {
    let (root, catalog) = fixture();
    fs::write(
        root.path().join("skills/review/references/check.md"),
        [0xff],
    )
    .unwrap();
    assert!(catalog.prompt(&config(SkillLoadingMode::Preload)).is_ok());
    assert!(
        catalog
            .prompt(&selected("review", "references/check.md"))
            .is_err()
    );
}
#[cfg(unix)]
#[test]
fn symlink_added_after_catalog_load_rejected_in_inventory() {
    let (root, catalog) = fixture();
    std::os::unix::fs::symlink(
        "/etc/passwd",
        root.path().join("skills/review/references/outside"),
    )
    .unwrap();
    assert!(catalog.prompt(&config(SkillLoadingMode::Preload)).is_err());
    assert!(
        catalog
            .prompt(&selected("review", "references/outside"))
            .is_err()
    );
}
