//! Run a trusted, fixed validation command against a recorded input/output pair.
use std::{process::Stdio, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

use crate::error::{DustError, Result};

const OUTPUT_LIMIT: usize = 8192;
const REASON_LIMIT: usize = 2048;

fn default_timeout() -> u64 {
    5000
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationMode {
    #[default]
    Legacy,
    Feedback,
}
impl ValidationMode {
    fn is_legacy(&self) -> bool {
        *self == Self::Legacy
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationDecision {
    Complete,
    Continue,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "ValidationMode::is_legacy")]
    pub mode: ValidationMode,
}

impl ValidationConfig {
    pub fn validate(&self) -> Result<()> {
        if self.command.trim().is_empty() || self.command.contains('\0') {
            return Err(DustError::Config(
                "validation command must be nonempty and contain no NUL".into(),
            ));
        }
        if self.args.iter().any(|arg| arg.contains('\0')) {
            return Err(DustError::Config(
                "validation arguments must contain no NUL".into(),
            ));
        }
        if !(1..=60000).contains(&self.timeout_ms) {
            return Err(DustError::Config(
                "validation timeout_ms must be between 1 and 60000".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationEvidence {
    pub passed: bool,
    pub reason: String,
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<ValidationDecision>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Verdict {
    passed: bool,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedbackVerdict {
    decision: ValidationDecision,
    reason: String,
}

fn failure(mode: ValidationMode, reason: &str, exit_code: Option<i32>) -> ValidationEvidence {
    ValidationEvidence {
        passed: false,
        reason: reason.into(),
        exit_code,
        decision: (mode == ValidationMode::Feedback).then_some(ValidationDecision::Blocked),
    }
}

async fn read_capped<R: AsyncRead + Unpin>(mut stream: R) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len() + count > OUTPUT_LIMIT {
            return Err(std::io::Error::other("validation output exceeds limit"));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

pub async fn validate(
    config: &ValidationConfig,
    input: &str,
    output: &str,
) -> Result<ValidationEvidence> {
    let payload = serde_json::json!({"input": input, "output": output});
    run(config, payload).await
}

pub async fn validate_execution(
    config: &ValidationConfig,
    input: &str,
    output: &str,
    tool_calls: &[super::execution::ToolRecord],
    state: &super::state::WorkingState,
) -> Result<ValidationEvidence> {
    validate_execution_in(
        config,
        input,
        output,
        tool_calls,
        state,
        &std::env::current_dir()?,
    )
    .await
}

pub async fn validate_execution_in(
    config: &ValidationConfig,
    input: &str,
    output: &str,
    tool_calls: &[super::execution::ToolRecord],
    state: &super::state::WorkingState,
    cwd: &std::path::Path,
) -> Result<ValidationEvidence> {
    let payload = if config.mode == ValidationMode::Feedback {
        serde_json::json!({"input": input, "output": output, "tool_calls": tool_calls, "state": state})
    } else {
        serde_json::json!({"input": input, "output": output})
    };
    run_in(config, payload, Some(cwd)).await
}

async fn run(config: &ValidationConfig, payload: serde_json::Value) -> Result<ValidationEvidence> {
    run_in(config, payload, None).await
}

async fn run_in(
    config: &ValidationConfig,
    payload: serde_json::Value,
    cwd: Option<&std::path::Path>,
) -> Result<ValidationEvidence> {
    config.validate()?;
    let mut command = Command::new(&config.command);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = match command
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            return Ok(failure(
                config.mode,
                "Validation command could not start",
                None,
            ));
        }
    };
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let payload = serde_json::to_vec(&payload)?;
    let operation = async {
        tokio::try_join!(
            async {
                stdin.write_all(&payload).await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, std::io::Error>(())
            },
            read_capped(stdout),
            read_capped(stderr),
            child.wait(),
        )
    };
    let result = tokio::time::timeout(Duration::from_millis(config.timeout_ms), operation).await;
    let (stdout, status) = match result {
        Ok(Ok(((), stdout, _stderr, status))) => (stdout, status),
        failed => {
            let _ = child.kill().await;
            let code = child.wait().await.ok().and_then(|status| status.code());
            return Ok(failure(
                config.mode,
                if failed.is_err() {
                    "Validation timed out"
                } else {
                    "Validation I/O failed or output exceeded limit"
                },
                code,
            ));
        }
    };
    if !status.success() {
        return Ok(failure(
            config.mode,
            "Validation command exited unsuccessfully",
            status.code(),
        ));
    }
    let (passed, reason, decision) = match config.mode {
        ValidationMode::Legacy => match serde_json::from_slice::<Verdict>(&stdout) {
            Ok(verdict) => (verdict.passed, verdict.reason, None),
            Err(_) => {
                return Ok(failure(
                    config.mode,
                    "Validation command returned an invalid verdict",
                    status.code(),
                ));
            }
        },
        ValidationMode::Feedback => match serde_json::from_slice::<FeedbackVerdict>(&stdout) {
            Ok(verdict) => (
                verdict.decision == ValidationDecision::Complete,
                verdict.reason,
                Some(verdict.decision),
            ),
            Err(_) => {
                return Ok(failure(
                    config.mode,
                    "Validation command returned an invalid verdict",
                    status.code(),
                ));
            }
        },
    };
    if reason.trim().is_empty() || reason.len() > REASON_LIMIT {
        return Ok(failure(
            config.mode,
            "Validation command returned an invalid reason",
            status.code(),
        ));
    }
    Ok(ValidationEvidence {
        passed,
        reason,
        exit_code: status.code(),
        decision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn python(script: &str) -> ValidationConfig {
        ValidationConfig {
            command: "python3".into(),
            args: vec!["-c".into(), script.into()],
            timeout_ms: 5000,
            mode: ValidationMode::Legacy,
        }
    }
    #[tokio::test]
    async fn validates_actual_pair() {
        let cfg = python(
            "import sys,json; d=json.load(sys.stdin); print(json.dumps({'passed':d=={'input':'request','output':'answer'},'reason':'pair checked'}))",
        );
        let evidence = validate(&cfg, "request", "answer").await.unwrap();
        assert!(evidence.passed);
        assert_eq!(evidence.exit_code, Some(0));
        assert!(!validate(&cfg, "request", "incorrect").await.unwrap().passed);
    }
    #[tokio::test]
    async fn rejects_nonzero_exit() {
        let cfg = python(
            "import json,sys; json.load(sys.stdin); print('{\"passed\":true,\"reason\":\"ok\"}'); sys.exit(3)",
        );
        let evidence = validate(&cfg, "", "").await.unwrap();
        assert!(!evidence.passed);
        assert_eq!(evidence.exit_code, Some(3));
    }
    #[tokio::test]
    async fn strict_verdict_and_reason() {
        for output in [
            "no json",
            "{\"passed\":true}",
            "{\"passed\":true,\"reason\":\"ok\",\"extra\":1}",
            "{\"passed\":true,\"reason\":\"\"}",
        ] {
            let cfg = python(&format!(
                "import sys,json; json.load(sys.stdin); print({output:?})"
            ));
            assert!(!validate(&cfg, "", "").await.unwrap().passed);
        }
        let cfg = python(
            "import sys,json; json.load(sys.stdin); print(json.dumps({'passed':True,'reason':'x'*2049}))",
        );
        assert!(!validate(&cfg, "", "").await.unwrap().passed);
    }
    #[tokio::test]
    async fn timeout_and_output_bounds() {
        let mut cfg = python("import time; time.sleep(10)");
        cfg.timeout_ms = 50;
        let evidence = validate(&cfg, "", "").await.unwrap();
        assert!(!evidence.passed);
        assert_eq!(evidence.reason, "Validation timed out");
        for script in [
            "import sys; sys.stdout.write('x'*9000)",
            "import sys; sys.stderr.write('x'*9000)",
        ] {
            assert!(!validate(&python(script), "", "").await.unwrap().passed);
        }
    }
    #[tokio::test]
    async fn early_exit_and_concurrent_stderr() {
        let cfg = python("import sys; sys.exit(0)");
        assert!(
            !validate(&cfg, &"x".repeat(1024 * 1024), "")
                .await
                .unwrap()
                .passed
        );
        let cfg = python(
            "import sys,json; json.load(sys.stdin); sys.stderr.write('x'*8192); print(json.dumps({'passed':True,'reason':'verified'}))",
        );
        let evidence = validate(&cfg, "", "").await.unwrap();
        assert!(evidence.passed);
        assert_eq!(evidence.reason, "verified");
    }

    #[tokio::test]
    async fn malformed_config_and_spawn_failure() {
        let mut cfg = python("");
        cfg.timeout_ms = 0;
        assert!(matches!(
            validate(&cfg, "", "").await,
            Err(DustError::Config(_))
        ));
        cfg.timeout_ms = 60001;
        assert!(cfg.validate().is_err());
        cfg.timeout_ms = 5000;
        cfg.command = "/definitely/no/validator".into();
        assert!(!validate(&cfg, "", "").await.unwrap().passed);
    }
}
