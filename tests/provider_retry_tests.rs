use dustagent::adapters::openai::OpenAiProvider;
use dustagent::application::retry::RetryConfig;
use dustagent::error::ProviderFailure;
use dustagent::{ChatMessage, DustError, LlmProvider};
use std::io::{Read, Write};
fn server(status: u16, body: &str, length: Option<usize>) -> String {
    server_with_retry_after(status, body, length, None)
}
fn server_with_retry_after(
    status: u16,
    body: &str,
    length: Option<usize>,
    retry_after: Option<&str>,
) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = body.to_owned();
    let retry_after = retry_after.map(str::to_owned);
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut b = [0];
            stream.read_exact(&mut b).unwrap();
            headers.push(b[0]);
        }
        let headers = String::from_utf8(headers).unwrap();
        let n = headers
            .lines()
            .find_map(|line| {
                let (k, v) = line.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let mut request = vec![0; n];
        stream.read_exact(&mut request).unwrap();
        let retry_header = retry_after
            .map(|value| format!("Retry-After: {value}\r\n"))
            .unwrap_or_default();
        write!(stream,"HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{retry_header}Connection: close\r\n\r\n{body}",length.unwrap_or(body.len())).unwrap();
    });
    format!("http://{addr}/v1")
}
#[tokio::test]
async fn adapter_preserves_retry_after_for_transient_status() {
    let provider = OpenAiProvider::with_config(
        "credential",
        server_with_retry_after(429, "quota", None, Some("3")),
        "fixture",
    );
    let error = provider
        .chat(&[ChatMessage::user("input")], None)
        .await
        .unwrap_err();
    assert!(error.is_retryable_provider_failure());
    assert!(matches!(
        error,
        DustError::ProviderRetryable {
            kind: ProviderFailure::Transient,
            retry_after: Some(delay),
            ..
        } if delay == std::time::Duration::from_secs(3)
    ));
}
#[tokio::test]
async fn adapter_distinguishes_status_invalid_json_and_interrupted_body() {
    for (status, body, length, kind) in [
        (
            503,
            "private-response-token",
            None,
            ProviderFailure::Transient,
        ),
        (429, "quota", None, ProviderFailure::Transient),
        (
            401,
            "private-response-token",
            None,
            ProviderFailure::Permanent,
        ),
        (400, "request", None, ProviderFailure::Permanent),
        (200, "not JSON", None, ProviderFailure::InvalidResponse),
        (200, "{}", None, ProviderFailure::InvalidResponse),
        (200, "{", Some(100), ProviderFailure::Transient),
    ] {
        let provider =
            OpenAiProvider::with_config("credential", server(status, body, length), "fixture");
        let error = provider
            .chat(&[ChatMessage::user("input")], None)
            .await
            .unwrap_err();
        assert!(
            matches!(&error,DustError::Provider {kind:observed,..} if *observed==kind),
            "{error}"
        );
        assert_eq!(
            error.is_retryable_provider_failure(),
            kind == ProviderFailure::Transient
        );
        assert!(!error.to_string().contains("private-response-token"));
    }
}
#[tokio::test]
async fn connection_failure_is_transient_but_legacy_llm_errors_are_not() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let provider =
        OpenAiProvider::with_config("credential", format!("http://{address}/v1"), "fixture");
    assert!(
        provider
            .chat(&[], None)
            .await
            .unwrap_err()
            .is_retryable_provider_failure()
    );
    assert!(!DustError::Llm("legacy unknown failure".into()).is_retryable_provider_failure());
}
#[test]
fn retry_config_rejects_unbounded_policies_and_caps_backoff() {
    let default = RetryConfig::default();
    assert_eq!(default.max_retries, 5);
    assert_eq!(default.base_delay_ms, 1000);
    assert!(default.is_default());
    assert!(default.validate().is_ok());
    assert_eq!(default.delay(0).as_millis(), 1000);
    assert_eq!(default.delay(1).as_millis(), 2000);
    assert_eq!(
        RetryConfig {
            max_retries: 5,
            base_delay_ms: 10000
        }
        .delay(usize::MAX)
        .as_millis(),
        10000
    );
    for config in [
        RetryConfig {
            max_retries: 6,
            base_delay_ms: 250,
        },
        RetryConfig {
            max_retries: 2,
            base_delay_ms: 0,
        },
        RetryConfig {
            max_retries: 2,
            base_delay_ms: 10001,
        },
    ] {
        assert!(config.validate().is_err());
    }
    assert_eq!(serde_json::from_str::<RetryConfig>("{}").unwrap(), default);
    assert!(serde_json::from_str::<RetryConfig>(r#"{"unknown":1}"#).is_err());
}
