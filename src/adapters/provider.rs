//! CLI provider selection. Explicit API configuration never falls back to account auth.
use super::{codex::CodexProvider, codex_auth::CodexAuth, openai::OpenAiProvider};
use crate::{ChatMessage, DustError, LlmProvider, LlmResponse, Result, ToolDefinition};
use async_trait::async_trait;

pub const DEFAULT_CODEX_MODEL: &str = "gpt-6.1-sol";
pub const DEFAULT_API_MODEL: &str = "gpt-4o-mini";

pub enum AutoProvider {
    OpenAi(OpenAiProvider),
    Codex(CodexProvider),
}

impl AutoProvider {
    pub fn new(model: Option<String>) -> Result<Self> {
        let key = setting("OPENAI_API_KEY")?;
        let base = setting("OPENAI_BASE_URL")?;
        if let Some(key) = key {
            return Ok(Self::OpenAi(OpenAiProvider::with_config(
                key,
                base.unwrap_or_else(|| "https://api.openai.com/v1".into()),
                model.unwrap_or_else(|| DEFAULT_API_MODEL.into()),
            )));
        }
        if base.is_some() {
            return Err(DustError::Config(
                "OPENAI_BASE_URL is set but OPENAI_API_KEY is missing; set a key (or a local-server placeholder), or unset the URL to use Codex login.".into(),
            ));
        }
        if let Some(key) = CodexAuth::cached_api_key()? {
            return Ok(Self::OpenAi(OpenAiProvider::with_config(
                key,
                "https://api.openai.com/v1",
                model.unwrap_or_else(|| DEFAULT_API_MODEL.into()),
            )));
        }
        Ok(Self::Codex(CodexProvider::new(
            model.unwrap_or_else(|| DEFAULT_CODEX_MODEL.into()),
        )?))
    }

    pub fn model(&self) -> &str {
        match self {
            Self::OpenAi(provider) => provider.model(),
            Self::Codex(provider) => provider.model(),
        }
    }
}

fn setting(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) if value.trim().is_empty() => Err(DustError::Config(format!(
            "{name} is set but empty; remove it or provide a value."
        ))),
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(DustError::Config(format!(
            "{name} must contain UTF-8 text."
        ))),
    }
}

#[async_trait]
impl LlmProvider for AutoProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LlmResponse> {
        match self {
            Self::OpenAi(provider) => provider.chat(messages, tools).await,
            Self::Codex(provider) => provider.chat(messages, tools).await,
        }
    }
}
