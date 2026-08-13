//! Which model an agent runs on, and how Velm reaches it.
//!
//! Feature 16's hard requirement is that this is a **per-node** choice: one board with one
//! agent on Claude, one on a model running on the user's own GPU and one on Kimi, at the
//! same time. So a provider is a value stored on the node, not a mode the application is in.
//!
//! # The four transports, and why there are four
//!
//! - [`Transport::ClaudeCli`] delegates to the `claude` binary the user already has, over
//!   **its own** line-delimited JSON protocol. It is the default for [`Provider::Claude`] and
//!   it is the answer to feature 17: that process already holds the user's Claude
//!   subscription, so Velm never sees a key and the user never pays per token.
//! - [`Transport::Acp`] delegates over the Agent Client Protocol, for agents that speak it.
//! - [`Transport::Pty`] runs a CLI in a real pseudo-terminal. For agents with no protocol
//!   mode, and for when the user wants the terminal itself on the board.
//! - [`Transport::Http`] talks to an API directly. This is how Kimi, a bare OpenAI key and
//!   **any OpenAI-compatible local server** are supported without a fifth code path —
//!   llama.cpp, LM Studio, Ollama and vLLM all speak it.
//!
//! ## ⚠ What was assumed here, and what is now measured
//!
//! This file used to say that `claude`, `codex` and `gemini` all speak ACP, and that ACP is
//! *"the entire answer to feature 17"*. That was written from recall, before anything could be
//! run. **Measured on this machine on 2026-08-13: `claude` does not speak ACP at all** — see
//! [`crate::transport::claude_cli`], whose tests quote the captured session. So Claude has its
//! own transport, and the two claims that stand up are the narrow ones: a delegated CLI is
//! unmetered, and the binary must be probed rather than believed in.
//!
//! `codex` and `gemini` are left on [`Transport::Acp`] and that is **unverified** — neither has
//! been run from here. It is a default, and a wrong default costs one config field
//! ([`ProviderChoice::transport`]), which is the whole reason that field exists.

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
    /// A delegated CLI wherever one exists, because the alternative bills the user for
    /// something they have already paid for. **Which** protocol that CLI speaks is not a
    /// guess for Claude and is one for the other two: `claude` was measured
    /// ([`Transport::ClaudeCli`]), `codex` and `gemini` are assumed to speak ACP and have
    /// never been run from here.
    pub const fn default_transport(self) -> Transport {
        match self {
            Self::Claude => Transport::ClaudeCli,
            Self::OpenAi | Self::Gemini => Transport::Acp,
            Self::Kimi | Self::Local | Self::Custom => Transport::Http,
        }
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

    /// The command a delegated session launches, when the provider has a known one —
    /// everything [`Transport::needs_a_command`] answers `true` for.
    ///
    /// **A default, not a fact.** The exact binary name and flags belong to tools that ship on
    /// their own schedule, so [`crate::transport::LaunchSpec::command`] overrides this and the
    /// session probes for the binary before trusting either. A wrong guess must cost a config
    /// field, never a broken feature — so a missing binary answers
    /// [`crate::AgentError::MissingCommand`], which names what was looked for and is the one
    /// error in this crate whose remedy is in its own text.
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
    /// The `claude` CLI's own line-delimited JSON protocol over stdio. The default for
    /// [`Provider::Claude`], and the one transport here whose wire format was **captured from
    /// a real session** rather than recalled — see [`crate::transport::claude_cli`].
    #[default]
    ClaudeCli,
    /// Agent Client Protocol: JSON-RPC 2.0 over a child process's stdio. For agents that
    /// genuinely speak it — which, measured, does not include `claude`.
    Acp,
    /// A real pseudo-terminal running a CLI.
    Pty,
    /// A direct HTTP API call.
    Http,
}

impl Transport {
    pub const ALL: [Self; 4] = [Self::ClaudeCli, Self::Acp, Self::Pty, Self::Http];

    /// The on-disk tag. Stable: it is written into board files.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::ClaudeCli => "claude_cli",
            Self::Acp => "acp",
            Self::Pty => "pty",
            Self::Http => "http",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            // Named for the product the user installed, not for the protocol: "Claude Code"
            // is what they signed into, and the wire format is our problem rather than theirs.
            Self::ClaudeCli => "Claude Code",
            Self::Acp => "Agent protocol",
            Self::Pty => "Terminal",
            Self::Http => "API",
        }
    }

    /// Whether this transport bills per token against an API key.
    ///
    /// What the interface uses to say *"this one runs on your subscription"* beside a node's
    /// provider row, which is the single most useful thing to know before starting a long run.
    ///
    /// [`Self::ClaudeCli`] is **not** metered, and that is the measured half rather than the
    /// hopeful one: the CLI authenticates with the user's own subscription credentials and
    /// Velm never sets `ANTHROPIC_API_KEY`. A `total_cost_usd` does appear on every `result`
    /// line — it is what the same tokens *would* cost on the API, and surfacing it as a bill
    /// would contradict the one claim this predicate exists to make.
    pub const fn is_metered(self) -> bool {
        matches!(self, Self::Http)
    }

    /// Whether this transport runs a child process Velm has to find first.
    ///
    /// The three that do all fail the same way — *"`claude` is not installed"* — and it is
    /// [`crate::transport::probe_command`] that turns that into a named message rather than a
    /// spawn error or a hang.
    pub const fn needs_a_command(self) -> bool {
        !matches!(self, Self::Http)
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

    /// The bring-your-own-subscription path: a provider with a CLI defaults to *delegating to
    /// it*, and no delegated transport is metered. If this ever inverts, every user of a Max
    /// plan starts paying twice.
    #[test]
    fn a_subscription_provider_defaults_to_an_unmetered_transport() {
        for provider in [Provider::Claude, Provider::OpenAi, Provider::Gemini] {
            assert!(provider.supports_subscription(), "{provider:?}");
            let choice = ProviderChoice::new(provider);
            assert!(
                !choice.effective_transport().is_metered(),
                "{provider:?} was billed per token by default"
            );
            assert!(!choice.is_metered(), "{provider:?} was billed per token by default");
        }
    }

    /// ⚠ **Claude's default is the transport that was measured, not the one that was
    /// remembered.** `claude` does not speak ACP — that was established by running it, and
    /// this assertion is what stops the old assumption being reinstated by someone tidying the
    /// enum. The other two keep ACP, which is a *guess* and is labelled one.
    #[test]
    fn claude_delegates_over_the_protocol_its_cli_actually_speaks() {
        let claude = ProviderChoice::new(Provider::Claude);
        assert_eq!(claude.effective_transport(), Transport::ClaudeCli);
        assert!(!claude.is_metered(), "the subscription path must never be billed");
        assert_eq!(claude.summary(), "Claude · default model · subscription");

        for guessed in [Provider::OpenAi, Provider::Gemini] {
            assert_eq!(guessed.default_transport(), Transport::Acp, "{guessed:?}");
        }

        // A node may still be pointed at ACP by hand — nothing about the measurement removes
        // a transport, and an agent that does speak ACP is still reachable.
        let by_hand = claude.clone().with_transport(Transport::Acp);
        assert_eq!(by_hand.effective_transport(), Transport::Acp);
        assert!(!by_hand.is_metered());
    }

    /// The tag is written into board files, so it must be exactly what serde writes — two
    /// spellings of one value is a board that reads back as a different transport than it was
    /// saved as. **A board saved before `ClaudeCli` existed carries `"acp"` and must keep
    /// parsing**, which is the only thing here that RULE ZERO cares about.
    #[test]
    fn every_transport_tag_is_the_one_serde_writes_and_the_old_ones_still_parse() {
        for transport in Transport::ALL {
            let json = serde_json::to_string(&transport).unwrap();
            assert_eq!(json, format!("\"{}\"", transport.tag()), "{transport:?}");
        }
        assert_eq!(Transport::ClaudeCli.tag(), "claude_cli");

        let older = r#"{"provider":"claude","transport":"acp"}"#;
        let choice: ProviderChoice = serde_json::from_str(older).unwrap();
        assert_eq!(choice.effective_transport(), Transport::Acp);
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
