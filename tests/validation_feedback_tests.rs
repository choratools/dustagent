use dustagent::application::{
    execution::{ToolRecord, ToolStatus},
    state::WorkingState,
    validation::{
        ValidationConfig, ValidationDecision, ValidationMode, validate, validate_execution,
    },
};

fn checker(script: &str, mode: ValidationMode) -> ValidationConfig {
    ValidationConfig {
        command: "python3".into(),
        args: vec!["-c".into(), script.into()],
        timeout_ms: 5000,
        mode,
    }
}
#[tokio::test]
async fn legacy_execution_receives_only_the_original_pair() {
    let config = checker(
        "import json,sys; d=json.load(sys.stdin); print(json.dumps({'passed':d=={'input':'task','output':'answer'},'reason':'pair checked'}))",
        ValidationMode::Legacy,
    );
    let evidence = validate_execution(&config, "task", "answer", &[], &WorkingState::default())
        .await
        .unwrap();
    assert!(evidence.passed);
    assert!(evidence.decision.is_none());
    let old = serde_json::json!({"command":"python3","args":[],"timeout_ms":5000});
    let config: ValidationConfig = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(config.mode, ValidationMode::Legacy);
    assert_eq!(serde_json::to_value(config).unwrap(), old);
}
#[tokio::test]
async fn feedback_receives_actual_tools_and_state() {
    let config = checker(
        "import json,sys; d=json.load(sys.stdin); assert set(d)=={'input','output','tool_calls','state'}; assert d['tool_calls'][0]['output']=='observed'; assert d['state']['entries']['cursor']==3; assert d['state']['revision']==1; print(json.dumps({'decision':'continue','reason':'fetch remaining pages'}))",
        ValidationMode::Feedback,
    );
    let tools = [ToolRecord {
        turn: 1,
        call_id: "c1".into(),
        name: "fetch".into(),
        arguments: serde_json::json!({}),
        status: ToolStatus::Succeeded,
        elapsed_ms: 0,
        output: Some("observed".into()),
        error: None,
        truncated: false,
    }];
    let state = WorkingState {
        entries: std::collections::BTreeMap::from([("cursor".into(), serde_json::json!(3))]),
        revision: 1,
    };
    let evidence = validate_execution(&config, "task", "candidate", &tools, &state)
        .await
        .unwrap();
    assert_eq!(evidence.decision, Some(ValidationDecision::Continue));
    assert!(!evidence.passed);
}
#[tokio::test]
async fn feedback_all_decisions_and_pair_review_contract() {
    for (decision, expected) in [
        ("complete", ValidationDecision::Complete),
        ("continue", ValidationDecision::Continue),
        ("blocked", ValidationDecision::Blocked),
    ] {
        let config = checker(
            &format!(
                "import json,sys; d=json.load(sys.stdin); assert set(d)=={{'input','output'}}; print(json.dumps({{'decision':'{decision}','reason':'checked'}}))"
            ),
            ValidationMode::Feedback,
        );
        let evidence = validate(&config, "task", "answer").await.unwrap();
        assert_eq!(evidence.decision, Some(expected));
        assert_eq!(evidence.passed, expected == ValidationDecision::Complete);
    }
}
#[tokio::test]
async fn malformed_error_timeout_and_nonzero_feedback_are_blocked() {
    for verdict in [
        "invalid",
        r#"{"decision":"continue"}"#,
        r#"{"decision":"retry","reason":"missing"}"#,
        r#"{"decision":"continue","reason":""}"#,
        r#"{"decision":"continue","reason":"ok","extra":1}"#,
        r#"{"passed":true,"reason":"ok"}"#,
    ] {
        let config = checker(
            &format!("import json,sys; json.load(sys.stdin); print({verdict:?})"),
            ValidationMode::Feedback,
        );
        let evidence = validate(&config, "", "").await.unwrap();
        assert!(!evidence.passed);
        assert_eq!(evidence.decision, Some(ValidationDecision::Blocked));
    }
    let config = checker(
        "import json,sys; json.load(sys.stdin); print('{\"decision\":\"continue\",\"reason\":\"retry\"}'); sys.exit(2)",
        ValidationMode::Feedback,
    );
    assert_eq!(
        validate(&config, "", "").await.unwrap().decision,
        Some(ValidationDecision::Blocked)
    );
    let mut config = checker("import time; time.sleep(10)", ValidationMode::Feedback);
    config.timeout_ms = 50;
    let evidence = validate(&config, "", "").await.unwrap();
    assert_eq!(evidence.reason, "Validation timed out");
    assert_eq!(evidence.decision, Some(ValidationDecision::Blocked));
    config.command = "/does/not/exist".into();
    assert_eq!(
        validate(&config, "", "").await.unwrap().decision,
        Some(ValidationDecision::Blocked)
    );
}
