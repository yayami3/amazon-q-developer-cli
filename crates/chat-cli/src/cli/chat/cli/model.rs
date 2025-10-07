use amzn_codewhisperer_client::types::Model;
use clap::Args;
use crossterm::style::{
    self,
    Color,
};
use crossterm::{
    execute,
    queue,
};
use dialoguer::Select;
use serde::{
    Deserialize,
    Serialize,
};

use crate::api_client::Endpoint;
use crate::cli::chat::{
    ChatError,
    ChatSession,
    ChatState,
};
use crate::os::Os;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Display name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// Description of the model
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Actual model id to send in the API
    pub model_id: String,
    /// Size of the model's context window, in tokens
    #[serde(default = "default_context_window")]
    pub context_window_tokens: usize,
    /// Rate multiplier for pricing (e.g., 1.5 means 1.5x the base rate)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_multiplier: Option<f64>,
    /// Unit for the rate multiplier (e.g., "tokens")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_unit: Option<String>,
}

impl ModelInfo {
    pub fn from_api_model(model: &Model) -> Self {
        let context_window_tokens = model
            .token_limits()
            .and_then(|limits| limits.max_input_tokens())
            .map_or(default_context_window(), |tokens| tokens as usize);
        Self {
            model_id: model.model_id().to_string(),
            description: model.description.clone(),
            model_name: model.model_name().map(|s| s.to_string()),
            context_window_tokens,
            rate_multiplier: model.rate_multiplier(),
            rate_unit: model.rate_unit().map(|s| s.to_string()),
        }
    }

    /// create a default model with only valid model_id（be compatoble with old stored model data）
    pub fn from_id(model_id: String) -> Self {
        Self {
            model_id,
            description: None,
            model_name: None,
            context_window_tokens: 200_000,
            rate_multiplier: None,
            rate_unit: None,
        }
    }

    pub fn display_name(&self) -> &str {
        self.model_name.as_deref().unwrap_or(&self.model_id)
    }

    pub fn display_name_with_pricing(&self) -> String {
        let base_name = self.display_name();
        if let Some(rate) = self.rate_multiplier {
            // Only show pricing if rate is greater than 1.0
            if rate > 1.0 {
                return format!("{} (pricing: {}x)", base_name, rate);
            }
        }
        base_name.to_string()
    }

    pub fn description(&self) -> Option<&str> {
        self.description
            .as_deref()
            .and_then(|d| if d.is_empty() { None } else { Some(d) })
    }
}

/// Command-line arguments for model selection operations
#[deny(missing_docs)]
#[derive(Debug, PartialEq, Args)]
pub struct ModelArgs;
impl ModelArgs {
    pub async fn execute(self, os: &Os, session: &mut ChatSession) -> Result<ChatState, ChatError> {
        Ok(select_model(os, session).await?.unwrap_or(ChatState::PromptUser {
            skip_printing_tools: false,
        }))
    }
}

pub async fn select_model(os: &Os, session: &mut ChatSession) -> Result<Option<ChatState>, ChatError> {
    queue!(session.stderr, style::Print("\n"))?;

    // Fetch available models from service
    let (models, _default_model) = get_available_models(os).await?;

    if models.is_empty() {
        queue!(
            session.stderr,
            style::SetForegroundColor(Color::Red),
            style::Print("No models available\n"),
            style::ResetColor
        )?;
        return Ok(None);
    }

    let active_model_id = session.conversation.model_info.as_ref().map(|m| m.model_id.as_str());

    let labels: Vec<String> = models
        .iter()
        .map(|model| {
            let display_name = model.display_name_with_pricing();
            let description = model.description();
            if Some(model.model_id.as_str()) == active_model_id {
                if let Some(desc) = description {
                    format!("{} (active) | {}", display_name, desc)
                } else {
                    format!("{} (active)", display_name)
                }
            } else if let Some(desc) = description {
                format!("{} | {}", display_name, desc)
            } else {
                display_name.to_string()
            }
        })
        .collect();

    let selection: Option<_> = match Select::with_theme(&crate::util::dialoguer_theme())
        .with_prompt("Select a model for this chat session")
        .items(&labels)
        .default(0)
        .interact_on_opt(&dialoguer::console::Term::stdout())
    {
        Ok(sel) => {
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::style::SetForegroundColor(crossterm::style::Color::Magenta)
            );
            sel
        },
        // Ctrl‑C -> Err(Interrupted)
        Err(dialoguer::Error::IO(ref e)) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(None),
        Err(e) => return Err(ChatError::Custom(format!("Failed to choose model: {e}").into())),
    };

    queue!(session.stderr, style::ResetColor)?;

    if let Some(index) = selection {
        let selected = models[index].clone();
        session.conversation.model_info = Some(selected.clone());
        let display_name = selected.display_name();

        queue!(
            session.stderr,
            style::Print("\n"),
            style::Print(format!(" Using {}\n\n", display_name)),
            style::ResetColor,
            style::SetForegroundColor(Color::Reset),
            style::SetBackgroundColor(Color::Reset),
        )?;
    }

    execute!(session.stderr, style::ResetColor)?;

    Ok(Some(ChatState::PromptUser {
        skip_printing_tools: false,
    }))
}

pub async fn get_model_info(model_id: &str, os: &Os) -> Result<ModelInfo, ChatError> {
    let (models, _) = get_available_models(os).await?;

    models
        .into_iter()
        .find(|m| m.model_id == model_id)
        .ok_or_else(|| ChatError::Custom(format!("Model '{}' not found", model_id).into()))
}

/// Get available models with caching support
pub async fn get_available_models(os: &Os) -> Result<(Vec<ModelInfo>, ModelInfo), ChatError> {
    let endpoint = Endpoint::configured_value(&os.database);
    let region = endpoint.region().as_ref();

    match os.client.get_available_models(region).await {
        Ok(api_res) => {
            let models: Vec<ModelInfo> = api_res.models.iter().map(ModelInfo::from_api_model).collect();
            let default_model = ModelInfo::from_api_model(&api_res.default_model);

            tracing::debug!("Successfully fetched {} models from API", models.len());
            Ok((models, default_model))
        },
        // In case of API throttling or other errors, fall back to hardcoded models
        Err(e) => {
            tracing::error!("Failed to fetch models from API: {}, using fallback list", e);

            let models = get_fallback_models();
            let default_model = models[0].clone();

            Ok((models, default_model))
        },
    }
}

/// Returns the context window length in tokens for the given model_id.
/// Uses cached model data when available
pub fn context_window_tokens(model_info: Option<&ModelInfo>) -> usize {
    model_info.map_or_else(default_context_window, |m| m.context_window_tokens)
}

fn default_context_window() -> usize {
    200_000
}

fn get_fallback_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            model_name: Some("claude-sonnet-4".to_string()),
            model_id: "claude-sonnet-4".to_string(),
            description: None,
            context_window_tokens: 200_000,
            rate_multiplier: None,
            rate_unit: None,
        },
        ModelInfo {
            model_name: Some("claude-3.7-sonnet".to_string()),
            model_id: "claude-3.7-sonnet".to_string(),
            description: None,
            context_window_tokens: 200_000,
            rate_multiplier: None,
            rate_unit: None,
        },
    ]
}

pub fn normalize_model_name(name: &str) -> &str {
    match name {
        "claude-4-sonnet" => "claude-sonnet-4",
        // can add more mapping for backward compatibility
        _ => name,
    }
}

pub fn find_model<'a>(models: &'a [ModelInfo], name: &str) -> Option<&'a ModelInfo> {
    let normalized = normalize_model_name(name);
    models.iter().find(|m| {
        m.model_name
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case(normalized))
            || m.model_id.eq_ignore_ascii_case(normalized)
    })
}

#[cfg(test)]
mod pricing_tests {
    use super::*;

    #[test]
    fn test_display_name_with_pricing() {
        // Test case 1: Model with pricing > 1.0 should show pricing
        let model_with_pricing = ModelInfo {
            model_name: Some("claude-sonnet-4.5".to_string()),
            description: Some("High-performance model".to_string()),
            model_id: "claude-sonnet-4.5".to_string(),
            context_window_tokens: 200_000,
            rate_multiplier: Some(1.5),
            rate_unit: Some("tokens".to_string()),
        };
        assert_eq!(
            model_with_pricing.display_name_with_pricing(),
            "claude-sonnet-4.5 (pricing: 1.5x)"
        );

        // Test case 2: Model with standard pricing (1.0) should not show pricing
        let model_standard = ModelInfo {
            model_name: Some("claude-sonnet-4".to_string()),
            description: Some("Standard model".to_string()),
            model_id: "claude-sonnet-4".to_string(),
            context_window_tokens: 200_000,
            rate_multiplier: Some(1.0),
            rate_unit: Some("tokens".to_string()),
        };
        assert_eq!(
            model_standard.display_name_with_pricing(),
            "claude-sonnet-4"
        );

        // Test case 3: Model with no pricing info should not show pricing
        let model_no_pricing = ModelInfo {
            model_name: Some("claude-3.7-sonnet".to_string()),
            description: Some("Previous generation".to_string()),
            model_id: "claude-3.7-sonnet".to_string(),
            context_window_tokens: 200_000,
            rate_multiplier: None,
            rate_unit: None,
        };
        assert_eq!(
            model_no_pricing.display_name_with_pricing(),
            "claude-3.7-sonnet"
        );
    }
}
