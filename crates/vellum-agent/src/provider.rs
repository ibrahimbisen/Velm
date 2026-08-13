//! Which model an agent runs on, and how Velm reaches it.
//!
//! Feature 16's hard requirement is that this is a **per-node** choice: one board with one
//! agent on Claude, one on a model running on the user's own GPU and one on Kimi, at the
//! same time. So a provider is a value stored on the node, not a mode the application is in.
//!
//! # The three transports, and why there are three
//!
//! - [`Transport::Acp`] delegates to an agent process the user already has — `claude`,
//!   `codex`, `gemini`. It is the default for those, and it is the entire answer to
//!   feature 17: that process already holds a Claude Max or Codex Pro subscription, so
//!   Velm never sees a key and the user never pays per token.
//! - [`Transport::Pty`] runs a CLI in a real pseudo-terminal. For agents with no protocol
//!   mode, and for when the user wants the terminal itself on the board.
//! - [`Transport::Http`] talks to an API directly. This is how Kimi, a bare OpenAI key and
//!   **any OpenAI-compatible local server** are supported without a fourth code path —
//!   llama.cpp, LM Studio, Ollama and vLLM all speak it.

use serde::{Deserialize, Serialize};

/// A provider Velm knows how to reach.
///
/// A closed enum rather than a free string, because each variant carries real knowledge —
/// which transport is right, what the default model is called, whether a key is needed at
/// all. [`Provider::Custom`] is the escape hatch, and it is what makes "adding further
/// providers straightforward" true today rather than at the next release: an
/// OpenAI-compatible endpoint needs no code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// Anthropic's Claude, through the `claude` CLI by default — which is what makes a
    /// Claude Max subscription work with no API key.
    #[default]
    Claude,
    /// OpenAI, through the `codex` CLI or the API.
    OpenAi,
    /// Moonshot's Kimi. API only; there is no first-party CLI to delegate to.
    Kimi,
    /// Google's Gemini, through the `gemini` CLI or the API.
    Gemini,
    /// A model on the user's own machine, over an OpenAI-compatible endpoint.
    Local,
    /// Anything else that speaks an OpenAI-compatible API, named by the user.
    Custom,
}

impl Provider {
    pub const ALL: [Self; 6] =
        [Self::Claude, Self::OpenAi, Self::Kimi, Self::Gemini, Self::Local, Self::Custom];

    /// The on-disk tag. Stable: it is written into board files.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "openai",
            Self::Kimi => "kimi",
            Self::Gemini => "gemini",
            Self::Local => "local",
            Self::Custom => "custom",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::OpenAi => "OpenAI",
            Self::Kimi => "Kimi",
            Self::Gemini => "Gemini",
            Self::Local => "Local model",
            Self::Custom => "Custom endpoint",
        }
    }

    /// Whether this provider can run through an agent process the user already signed in to.
    ///
    /// This is the bring-your-own-subscription predicate. It is *whether a CLI exists to
    /// delegate to*, not whether one is installed — installation is probed at session start,
    /// because a missing binary must degrade to a named toast rather than a wrong assumption.
    pub const fn supports_subscription(self) -> bool {
        matches!(self, Self::Claude | Self::OpenAi | Self::Gemini)
    }

    /// The transport to use when the user has not chosen one.
    ///
    /// ACP wherever a subscription-holding CLI exists, because the alternative bills the
    /// user for something they have already paid for.
    pub const fn default_transport(self) -> Transport {
        if self.supports_subscription() { Transport::Acp } else { Transport::Http }
    }

    /// The default API base, for the providers that have a fixed one.
    ///
    /// `None` for [`Provider::Local`] and [`Provider::Custom`]: those *are* their endpoint,
    /// and guessing `localhost:11434` would silently talk to whichever of the four local
    /// servers happened to be running.
    pub const fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("https://api.anthropic.com"),
            Self::OpenAi => Some("https://api.openai.com/v1"),
            Self::Kimi => Some("https://api.moonshot.ai/v1"),
            Self::Gemini => Some("https://generativelanguage.googleapis.com"),
            Self::Local | Self::Custom => None,
        }
    }

    /// The command an ACP or PTY session launches, when the provider has a known one.
    ///
    /// **A default, not a fact.** The exact binary name and flags belong to tools that ship
    /// on their own schedule, so [`ProviderConfig::command`] overrides this and the session
    /// probes for the binary before trusting either. A wrong guess must cost a config field,
    /// never a broken feature — so a missing binary degrades to [`Transport::Http`] with a
    /// toast naming what was not found.
    pub const fn default_command(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("claude"),
            Self::OpenAi => Some("codex"),
            Self::Gemini => Some("gemini"),
            Self::Kimi | Self::Local | Self::Custom => None,
        }
    }

    /// Whether an API key is required to use this provider over [`Transport::Http`].
    ///
    /// A local model needs none, which is the point of it.
    pub const fn needs_api_key(self) -> bool {
        !matches!(self, Self::Local)
    }
}

/// How Velm talks to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Agent Client Protocol: JSON-RPC 2.0 over a child process's stdio. The default where
    /// a subscription-holding CLI exists.
    #[default]
    Acp,
    /// A real pseudo-terminal running a CLI.
    Pty,
    /// A direct HTTP API call.
    Http,
}

impl Transport {
    pub const ALL: [Self; 3] = [Self::Acp, Self::Pty, Self::Http];

    pub const fn tag(self) -> &'static str {
        match self {
            Self::Acp => "acp",
            Self::Pty => "pty",
            Self::Http => "http",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Acp => "Agent protocol",
            Self::Pty => "Terminal",
            Self::Http => "API",
        }
    }

    /// Whether this transport bills per token against an API key.
    ///
    /// What the interface uses to say *"this one runs on your subscription"* beside a node's
    /// provider row, which is the single most useful thing to know before starting a long run.
    pub const fn is_metered(self) -> bool {
        matches!(self, Self::Http)
    }
}

/// The provider a node runs on: which one, which model, and how to reach it.
///
/// Stored on the node — feature 16's per-agent requirement — and small, because most nodes
/// will carry only a provider tag and inherit the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProviderChoice {
    pub provider: Provider,
    /// The model name, verbatim as the provider spells it. `None` uses whatever the
    /// provider or the delegated CLI considers current — which is deliberately *not* a
    /// hardcoded model id here: a pinned default is wrong within months, and a board that
    /// names no model keeps working.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `None` takes [`Provider::default_transport`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
}

impl ProviderChoice {
    pub fn new(provider: Provider) -> Self {
        Self { provider, model: None, transport: None }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = Some(transport);
        self
    }

    /// The transport actually used, resolving `None` through the provider.
    pub fn effective_transport(&self) -> Transport {
        self.transport.unwrap_or_else(|| self.provider.default_transport())
    }

    /// Whether this choice bills per token.
    pub fn is_metered(&self) -> bool {
        self.effective_transport().is_metered() && self.provider.needs_api_key()
    }

    /// A one-line description for the node's header: `"Claude · subscription"`.
    pub fn summary(&self) -> String {
        let model = self.model.as_deref().unwrap_or("default model");
        let billing = if self.is_metered() { "API" } else { "subscription" };
        format!("{} · {model} · {billing}", self.provider.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bring-your-own-subscription path: a provider with a CLI defaults to ACP, and ACP
    /// is not metered. If this ever inverts, every user of a Max plan starts paying twice.
    #[test]
    fn a_subscription_provider_defaults_to_an_unmetered_transport() {
        for provider in [Provider::Claude, Provider::OpenAi, Provider::Gemini] {
            assert!(provider.supports_subscription(), "{provider:?}");
            let choice = ProviderChoice::new(provider);
            assert_eq!(choice.effective_transport(), Transport::Acp, "{provider:?}");
            assert!(!choice.is_metered(), "{provider:?} was billed per token by default");
        }
    }

    /// Kimi has no CLI to delegate to, so it is an API call and it says so.
    #[test]
    fn a_provider_with_no_cli_falls_back_to_the_api() {
        let kimi = ProviderChoice::new(Provider::Kimi);
        assert_eq!(kimi.effective_transport(), Transport::Http);
        assert!(kimi.is_metered());
        assert!(kimi.summary().ends_with("API"), "{}", kimi.summary());
    }

    /// A local model is HTTP but not metered — it is the user's own GPU. Reporting it as
    /// billed would be a lie in the one place the interface exists to be trusted.
    #[test]
    fn a_local_model_is_never_reported_as_billed() {
        let local = ProviderChoice::new(Provider::Local).with_model("qwen3-coder");
        assert_eq!(local.effective_transport(), Transport::Http);
        assert!(!local.provider.needs_api_key());
        assert!(!local.is_metered());
        assert_eq!(local.summary(), "Local model · qwen3-coder · subscription");
    }

    /// No model id is hardcoded as a default. A pinned one is wrong within months and would
    /// silently override what the delegated CLI already considers current.
    #[test]
    fn no_provider_pins_a_model_name() {
        for provider in Provider::ALL {
            assert_eq!(ProviderChoice::new(provider).model, None, "{provider:?}");
        }
    }

    /// Only the two that have no fixed home are without a base URL — those are the ones
    /// where guessing would silently talk to whichever local server happened to be running.
    #[test]
    fn only_user_supplied_endpoints_lack_a_base_url() {
        for provider in Provider::ALL {
            let has = provider.default_base_url().is_some();
            let expected = !matches!(provider, Provider::Local | Provider::Custom);
            assert_eq!(has, expected, "{provider:?}");
        }
    }

    #[test]
    fn a_choice_round_trips_and_stays_small() {
        let bare = ProviderChoice::new(Provider::Claude);
        assert_eq!(serde_json::to_string(&bare).unwrap(), r#"{"provider":"claude"}"#);

        let full = ProviderChoice::new(Provider::Custom)
            .with_model("mixtral")
            .with_transport(Transport::Http);
        let json = serde_json::to_string(&full).unwrap();
        assert_eq!(serde_json::from_str::<ProviderChoice>(&json).unwrap(), full);

        for transport in Transport::ALL {
            let json = serde_json::to_string(&transport).unwrap();
            assert_eq!(serde_json::from_str::<Transport>(&json).unwrap(), transport);
        }
    }
}
