//! ACP agent definitions — `~/.config/termesh/agents.toml` (ARCHITECTURE.md §13).
//!
//! Agents are **data, not code**. ADR-0003 promises agent-agnosticism, and we keep that
//! promise in the *default* rather than only in the abstraction: nothing ships hard-wired
//! to a vendor, and with no config the editor runs Tier 0 and says so.
//!
//! ```toml
//! default = "my-agent"
//!
//! [agents.my-agent]
//! command = ["some-agent", "--acp"]
//! ```
//!
//! Commands are **argv arrays**, never shell strings — the same rule that governs agent
//! tool calls (ARCHITECTURE.md §11), applied to our own spawning so there is one story
//! about how processes start.

use serde::Deserialize;

/// One configured agent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentConfig {
    /// argv. The first element is the program.
    pub command: Vec<String>,
}

/// The parsed `agents.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AgentsConfig {
    /// Which agent to start. When absent and exactly one is defined, that one is used —
    /// naming it twice for no reason is a papercut.
    pub default: Option<String>,
    #[serde(rename = "agents")]
    pub agents: std::collections::BTreeMap<String, AgentConfig>,
}

/// Why a config file could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// Surfaced to the user with the reason, never swallowed — ARCHITECTURE.md §13 asks
    /// for config errors to appear *inside* the app with the fallback taken.
    Invalid(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Invalid(why) => write!(f, "agents.toml: {why}"),
        }
    }
}

impl AgentsConfig {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let parsed: AgentsConfig =
            toml::from_str(text).map_err(|e| ConfigError::Invalid(e.message().to_string()))?;
        parsed.validate()?;
        Ok(parsed)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (name, agent) in &self.agents {
            if agent.command.is_empty() {
                return Err(ConfigError::Invalid(format!("agent '{name}' has an empty command")));
            }
        }
        if let Some(name) = &self.default {
            if !self.agents.contains_key(name) {
                return Err(ConfigError::Invalid(format!("default agent '{name}' is not defined")));
            }
        }
        Ok(())
    }

    /// The agent to start, if one can be determined.
    pub fn selected(&self) -> Option<(&str, &AgentConfig)> {
        if let Some(name) = &self.default {
            return self.agents.get_key_value(name).map(|(n, a)| (n.as_str(), a));
        }
        // Exactly one defined and no default named: that is obviously the one meant.
        // Two or more is genuinely ambiguous, so we do not guess.
        if self.agents.len() == 1 {
            return self.agents.iter().next().map(|(n, a)| (n.as_str(), a));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_config_means_no_agent_rather_than_a_default_vendor() {
        let config = AgentsConfig::default();
        assert_eq!(config.selected(), None, "ADR-0003: agnostic in the default too");
    }

    #[test]
    fn a_single_agent_needs_no_default_named() {
        let config = AgentsConfig::parse(
            r#"
            [agents.mine]
            command = ["some-agent", "--acp"]
            "#,
        )
        .unwrap();
        let (name, agent) = config.selected().unwrap();
        assert_eq!(name, "mine");
        assert_eq!(agent.command, ["some-agent", "--acp"]);
    }

    #[test]
    fn several_agents_without_a_default_is_not_guessed_at() {
        let config = AgentsConfig::parse(
            r#"
            [agents.a]
            command = ["a"]
            [agents.b]
            command = ["b"]
            "#,
        )
        .unwrap();
        assert_eq!(config.selected(), None, "ambiguous, so ask rather than pick");
    }

    #[test]
    fn a_named_default_wins() {
        let config = AgentsConfig::parse(
            r#"
            default = "b"
            [agents.a]
            command = ["a"]
            [agents.b]
            command = ["b", "--acp"]
            "#,
        )
        .unwrap();
        assert_eq!(config.selected().unwrap().0, "b");
    }

    #[test]
    fn a_default_naming_an_undefined_agent_is_an_error_not_a_silent_fallback() {
        let err = AgentsConfig::parse(
            r#"
            default = "ghost"
            [agents.a]
            command = ["a"]
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("ghost"), "the message names the problem: {err}");
    }

    #[test]
    fn an_empty_command_is_rejected() {
        let err = AgentsConfig::parse(
            r#"
            [agents.broken]
            command = []
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("broken"));
    }

    #[test]
    fn malformed_toml_reports_why() {
        let err = AgentsConfig::parse("this is not toml {{{").unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn an_empty_file_is_valid_and_selects_nothing() {
        assert_eq!(AgentsConfig::parse("").unwrap().selected(), None);
    }

    #[test]
    fn a_command_is_an_argv_array_so_quoting_never_has_to_be_guessed() {
        let config = AgentsConfig::parse(
            r#"
            [agents.mine]
            command = ["some agent", "--flag", "a value with spaces"]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.selected().unwrap().1.command,
            ["some agent", "--flag", "a value with spaces"]
        );
    }
}
