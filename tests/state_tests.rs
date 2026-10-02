use dustagent::application::state::WorkingState;
use serde_json::json;

#[test]
fn missing_null_and_revision() {
    let mut state = WorkingState::default();
    assert_eq!(
        state.get("missing").unwrap(),
        json!({"found":false,"value":null,"revision":0})
    );
    assert_eq!(
        state.put("key".into(), json!(null)).unwrap(),
        json!({"revision":1})
    );
    assert_eq!(
        state.get("key").unwrap(),
        json!({"found":true,"value":null,"revision":1})
    );
    state.put("key".into(), json!({"nested":[1,2]})).unwrap();
    assert_eq!(state.revision, 2);
}
#[test]
fn key_and_value_boundaries_reject_atomically() {
    let mut state = WorkingState::default();
    state
        .put("가".repeat(256), json!("x".repeat(16382)))
        .unwrap();
    let before = serde_json::to_value(&state).unwrap();
    for key in [String::new(), "x".repeat(257), "bad\0key".into()] {
        assert!(state.put(key, json!(1)).is_err());
    }
    assert!(
        state
            .put("too-large".into(), json!("x".repeat(16383)))
            .is_err()
    );
    assert_eq!(serde_json::to_value(&state).unwrap(), before);
    assert!(state.get("").is_err());
    assert!(state.list("", Some("")).is_err());
    assert!(state.list(&"x".repeat(257), None).is_err());
}
#[test]
fn entry_limit_and_replacement() {
    let mut state = WorkingState::default();
    for i in 0..256 {
        state.put(format!("key-{i:03}"), json!(i)).unwrap();
    }
    let revision = state.revision;
    assert!(state.put("extra".into(), json!(0)).is_err());
    assert_eq!(state.revision, revision);
    state.put("key-000".into(), json!("replacement")).unwrap();
    assert_eq!(state.entries.len(), 256);
}
#[test]
fn pagination_prefix_and_cursor() {
    let mut state = WorkingState::default();
    for i in 0..45 {
        state.put(format!("page/{i:03}"), json!(i)).unwrap();
    }
    state.put("other".into(), json!(true)).unwrap();
    let first = state.list("page/", None).unwrap();
    assert_eq!(first["keys"].as_array().unwrap().len(), 20);
    assert_eq!(first["next_cursor"], "page/019");
    let second = state.list("page/", first["next_cursor"].as_str()).unwrap();
    assert_eq!(second["keys"][0], "page/020");
    let third = state.list("page/", second["next_cursor"].as_str()).unwrap();
    assert_eq!(third["keys"].as_array().unwrap().len(), 5);
    assert!(third["next_cursor"].is_null());
    assert_eq!(third["revision"], 46);
    assert!(
        state.list("absent", None).unwrap()["keys"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn total_limit_and_revision_overflow() {
    let mut state = WorkingState::default();
    for i in 0..15 {
        state
            .put(format!("large-{i}"), json!("x".repeat(16382)))
            .unwrap();
    }
    let before = serde_json::to_value(&state).unwrap();
    assert!(
        state
            .put("large-16".into(), json!("x".repeat(16382)))
            .is_err()
    );
    assert_eq!(serde_json::to_value(&state).unwrap(), before);
    state.revision = u64::MAX;
    assert!(state.put("key".into(), json!(0)).is_err());
    assert_eq!(state.revision, u64::MAX);
    assert!(!state.entries.contains_key("key"));
}
#[test]
fn corrupt_loaded_state_validation() {
    let invalid: WorkingState =
        serde_json::from_value(json!({"revision":1,"entries":{"":1}})).unwrap();
    assert!(invalid.validate().is_err());
    let invalid: WorkingState =
        serde_json::from_value(json!({"revision":1,"entries":{"key":"x".repeat(16383)}})).unwrap();
    assert!(invalid.validate().is_err());
    assert!(
        serde_json::from_value::<WorkingState>(json!({"revision":0,"entries":{},"unknown":true}))
            .is_err()
    );
}
