use async_trait::async_trait;
use dustagent::{
    AppManifest, ChatMessage, DustCore, LlmProvider, LlmResponse, StopReason, ToolCall,
    ToolDefinition,
};
use std::sync::{Arc, Mutex};

/// Calls a built-in tool every turn until it is told to finish, recording each request.
#[derive(Clone, Default)]
struct Probe {
    requests: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    finish_at: Option<usize>,
}
#[async_trait]
impl LlmProvider for Probe {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        _: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        let turn = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(messages.to_vec());
            requests.len()
        };
        if self.finish_at == Some(turn) {
            Ok(LlmResponse::text(r#"{"status":"complete"}"#))
        } else {
            Ok(LlmResponse {
                content: None,
                tool_calls: Some(vec![ToolCall::new(
                    format!("call-{turn}"),
                    "dustagent__hash",
                    r#"{"input":"body"}"#,
                )]),
            })
        }
    }
}
fn manifest(max_turns: usize, notice: bool, wrap_up: bool) -> AppManifest {
    let mut manifest = AppManifest::new();
    manifest.max_turns = Some(max_turns);
    manifest.budget_notice = notice;
    manifest.wrap_up_on_limit = wrap_up.then_some(true);
    manifest
}
fn notices(request: &[ChatMessage]) -> Vec<String> {
    request
        .iter()
        .filter_map(|m| m.content.clone())
        .filter(|s| s.starts_with("[Run budget:"))
        .collect()
}

#[tokio::test]
async fn off_by_default_and_not_serialized() {
    let app = manifest(2, false, false);
    assert!(
        serde_json::to_value(&app)
            .unwrap()
            .get("budget_notice")
            .is_none()
    );
    let probe = Probe::default();
    let report = DustCore::new(app, probe.clone())
        .execute_report("Return JSON only")
        .await;
    assert_eq!(report.stop_reason, StopReason::TurnLimit);
    assert!(
        probe
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| notices(r).is_empty())
    );
}

#[tokio::test]
async fn each_request_gets_one_fresh_notice_that_is_not_kept() {
    let probe = Probe {
        finish_at: Some(3),
        ..Probe::default()
    };
    let report = DustCore::new(manifest(3, true, false), probe.clone())
        .execute_report("Return JSON only")
        .await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    let requests = probe.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for (index, request) in requests.iter().enumerate() {
        let found = notices(request);
        assert_eq!(found.len(), 1, "turn {}", index + 1);
        assert!(found[0].contains(&format!("Model turn {} of 3", index + 1)));
        // It is the last message of the request, after any tool results.
        assert_eq!(
            request.last().unwrap().content.as_deref(),
            Some(found[0].as_str())
        );
    }
    assert!(notices(&requests[2])[0].contains("final turn: do not call tools"));
    assert!(!notices(&requests[0])[0].contains("final turn"));
}

#[tokio::test]
async fn wrap_up_turns_are_reported_as_such() {
    let probe = Probe {
        finish_at: Some(5),
        ..Probe::default()
    };
    let report = DustCore::new(manifest(2, true, true), probe.clone())
        .execute_report("Return JSON only")
        .await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.turns_used, 5);
    let requests = probe.requests.lock().unwrap();
    let said = |turn: usize| notices(&requests[turn - 1]).remove(0);
    assert!(
        said(2).contains("Model turn 2 of 2; 0 more after this one, then up to 3 wrap-up turns")
    );
    assert!(!said(2).contains("final turn"));
    assert!(said(3).contains("Wrap-up turn 1 of 3"));
    assert!(
        said(5).contains("Wrap-up turn 3 of 3")
            && said(5).contains("final turn: do not call tools")
    );
}
