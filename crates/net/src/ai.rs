//! Which AI Spool asks, and asking it.
//!
//! Two can be connected: ChatGPT, through the sign-in OpenAI's Codex app uses
//! (`chatgpt`), and Claude, through an Anthropic API key (`claude`). Every
//! feature that wants a model — the writing tools, sorting mail by priority,
//! quick replies — goes through [`respond`], so which one answers is a
//! setting and not something each feature has to know about.

use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{chatgpt, claude};

/// Who answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// The first one Spool had, and what an install from before there was a
    /// choice was already using.
    #[default]
    ChatGpt,
    Claude,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::ChatGpt, Provider::Claude];

    /// The product, as people know it.
    pub fn name(self) -> &'static str {
        match self {
            Provider::ChatGpt => "ChatGPT",
            Provider::Claude => "Claude",
        }
    }

    /// Who the text goes to, for saying so.
    pub fn company(self) -> &'static str {
        match self {
            Provider::ChatGpt => "OpenAI",
            Provider::Claude => "Anthropic",
        }
    }
}

/// A model a provider offers, for Settings to list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub slug: String,
    /// What the provider calls it, e.g. "GPT-5.5" or "Claude Opus 5.5".
    pub name: String,
    pub description: Option<String>,
}

/// The models a provider offers, in the order it recommends them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offered {
    pub models: Vec<Model>,
    /// The one used when none is picked.
    pub default: Option<String>,
}

/// The provider, as a byte, so a request on any thread can read it without a
/// lock. Settings sets it at launch and whenever it changes.
static PROVIDER: AtomicU8 = AtomicU8::new(0);

pub fn set_provider(provider: Provider) {
    PROVIDER.store(provider as u8, Ordering::SeqCst);
}

pub fn provider() -> Provider {
    match PROVIDER.load(Ordering::SeqCst) {
        1 => Provider::Claude,
        _ => Provider::ChatGpt,
    }
}

/// Asks whichever provider is chosen to follow `instructions` on `prompt`,
/// and returns its text.
pub async fn respond(instructions: &str, prompt: &str) -> Result<String> {
    match provider() {
        Provider::ChatGpt => chatgpt::respond(instructions, prompt).await,
        Provider::Claude => claude::respond(instructions, prompt).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choice_is_written_the_way_settings_reads_it() {
        assert_eq!(serde_json::to_string(&Provider::ChatGpt).unwrap(), "\"chatgpt\"");
        assert_eq!(serde_json::to_string(&Provider::Claude).unwrap(), "\"claude\"");
        assert_eq!(serde_json::from_str::<Provider>("\"claude\"").unwrap(), Provider::Claude);
    }
}
