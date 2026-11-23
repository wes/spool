//! Asking Claude, with an Anthropic API key.
//!
//! Anthropic has no sign-in that lets another app spend a Claude.ai plan, the
//! way the ChatGPT sign-in spends a ChatGPT one. What it has is the API: a key
//! made in the Anthropic Console, each request billed to that account. So
//! this is the API, called directly — there is no Anthropic SDK for Rust, and
//! one HTTP request is all a reply needs.
//!
//! The key goes into the keychain and is read from there once; after that it
//! lives only in memory, as the ChatGPT access token does.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::ai::{Model, Offered};
use crate::http;
use crate::secrets::{self, Slot};

const API: &str = "https://api.anthropic.com/v1";

/// The API version every request is made against.
const VERSION: &str = "2026-10-21";

/// The model used when none is picked in Settings.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";

/// Server-side fallback: a request a model's safety checks turn down is
/// answered by another model instead of coming back empty. "default" lets the
/// API pick that model by why it was declined, so there is no list of model
/// names in here to go stale.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// The models that take `fallbacks: "default"`. Anything else is sent
/// without it rather than refused for having it.
const TAKES_FALLBACKS: [&str; 5] =
    ["claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5", "claude-fable-5-2", "claude-fable-5-1"];

/// Room for an email and then some. A reply that reaches it is cut off, and
/// is reported as that rather than sent on half-written.
const MAX_TOKENS: u32 = 16_000;

/// See `chatgpt::REPLY_TIMEOUT`. A little longer, because the reply arrives
/// whole rather than streamed.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

/// The keychain entry's account name: one key per install.
const KEYCHAIN_ACCOUNT: &str = "anthropic";

const NOT_CONNECTED: &str = "Add an Anthropic API key in Settings \u{2192} AI to use Claude.";
const REFUSED_KEY: &str = "Anthropic refused the API key. Check it in Settings \u{2192} AI.";

/// What Settings shows for a connected key: enough to tell two apart, not
/// enough to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// The last four characters, e.g. "…AbCd".
    pub hint: String,
}

impl Key {
    fn of(key: &str) -> Self {
        let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        Key { hint: format!("\u{2026}{tail}") }
    }
}

/// The key this install has, if any. Reads the keychain the first time, so
/// it belongs on a background thread.
pub fn connected() -> Result<Option<Key>> {
    Ok(key()?.as_deref().map(Key::of))
}

/// Checks `key` with Anthropic and, if it is accepted, keeps it.
///
/// Checked by listing the models, which costs nothing, rather than by asking
/// one something. A key that would only fail on the first real request would
/// be found out by a writing tool, a long way from where it was typed.
pub async fn connect(key: &str) -> Result<Key> {
    let key = key.trim();
    if key.is_empty() {
        bail!("Paste an API key first.");
    }
    if key.chars().any(char::is_whitespace) {
        bail!("That doesn\u{2019}t look like an API key: it has spaces in it.");
    }
    list_models(key).await?;
    secrets::store(Slot::Claude, KEYCHAIN_ACCOUNT, key).context("saving the API key to the keychain")?;
    if let Ok(mut cache) = cache().lock() {
        *cache = Some(key.to_string());
    }
    Ok(Key::of(key))
}

/// Forgets the key, here and in the keychain.
pub fn disconnect() -> Result<()> {
    if let Ok(mut cache) = cache().lock() {
        *cache = None;
    }
    secrets::delete(Slot::Claude, KEYCHAIN_ACCOUNT)
}

/// Uses `model` from now on, or [`DEFAULT_MODEL`] when `None`.
pub fn set_model(model: Option<String>) {
    if let Ok(mut preference) = preference().lock() {
        *preference = model.filter(|m| !m.trim().is_empty());
    }
}

/// The models the key can use, newest first, for Settings to list.
pub async fn models() -> Result<Offered> {
    let key = key()?.ok_or_else(|| anyhow!(NOT_CONNECTED))?;
    let models = list_models(&key).await?;
    let default = models.iter().any(|m| m.slug == DEFAULT_MODEL).then(|| DEFAULT_MODEL.to_string());
    Ok(Offered { default: default.or_else(|| models.first().map(|m| m.slug.clone())), models })
}

/// Asks Claude to follow `instructions` on `prompt`, and returns its text.
pub async fn respond(instructions: &str, prompt: &str) -> Result<String> {
    let work = async {
        let key = key()?.ok_or_else(|| anyhow!(NOT_CONNECTED))?;
        let model = preference().lock().ok().and_then(|p| p.clone()).unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let body = request_body(&model, instructions, prompt);

        let mut headers = headers(&key, true);
        if takes_fallbacks(&model) {
            headers.push(("anthropic-beta".into(), FALLBACK_BETA.into()));
        }
        let reply = http::request(
            "POST",
            &format!("{API}/messages"),
            &headers,
            Some(body.to_string().as_bytes()),
            MAX_REPLY_BYTES,
        )
        .await
        .context("talking to Claude")?;

        let text = String::from_utf8_lossy(&reply.body);
        match reply.status {
            200..=299 => text_of(&text),
            401 | 403 => bail!(REFUSED_KEY),
            status => bail!(failure(status, &text)),
        }
    };

    smol::future::or(work, async {
        smol::Timer::after(REPLY_TIMEOUT).await;
        bail!("Claude took too long to answer. Try again in a moment.")
    })
    .await
}

/// The body of a `respond` request.
///
/// Low effort: rewording an email, sorting mail and drafting a two-line reply
/// need little thought, and more of it takes several times as long to say
/// the same thing. Thinking itself is left at the model's default — the
/// current models think adaptively and refuse a request that turns it off.
fn request_body(model: &str, instructions: &str, prompt: &str) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": instructions,
        "messages": [{ "role": "user", "content": prompt }],
        "output_config": { "effort": "low" },
    });
    if takes_fallbacks(model) {
        body["fallbacks"] = json!("default");
    }
    body
}

fn takes_fallbacks(model: &str) -> bool {
    TAKES_FALLBACKS.contains(&model)
}

/// The text of a reply: its text blocks, in order. Thinking blocks are the
/// model's own and are not part of the answer.
fn text_of(body: &str) -> Result<String> {
    let reply: Value = serde_json::from_str(body).context("Claude\u{2019}s reply was not the JSON expected")?;
    match reply["stop_reason"].as_str() {
        // Declined by a safety check, on every model the request was allowed
        // to fall back to.
        Some("refusal") => bail!("Claude declined to answer this one."),
        // Cut off. Half an email is worse than none.
        Some("max_tokens") => bail!("Claude\u{2019}s answer ran too long and was cut off. Try again."),
        _ => {}
    }
    let text: String = reply["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    if text.trim().is_empty() {
        bail!("Claude sent back an empty answer. Try again.");
    }
    Ok(text)
}

/// What went wrong, in words someone can act on.
fn failure(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(|m| m.trim().to_string()))
        .filter(|m| !m.is_empty());
    match status {
        429 => "Claude is getting too many requests from this key. Try again in a minute.".into(),
        // Overloaded: on Anthropic's side, and usually brief.
        529 | 503 => "Claude is busy right now. Try again in a moment.".into(),
        _ => match detail {
            Some(detail) => format!("Claude couldn\u{2019}t do this: {detail}"),
            None => format!("Claude couldn\u{2019}t do this (HTTP {status})."),
        },
    }
}

/// Every model the key can use, as the API lists them: newest first.
async fn list_models(key: &str) -> Result<Vec<Model>> {
    let reply = http::request("GET", &format!("{API}/models?limit=100"), &headers(key, false), None, 1024 * 1024)
        .await
        .context("talking to Anthropic")?;
    let text = String::from_utf8_lossy(&reply.body);
    match reply.status {
        200..=299 => {}
        401 | 403 => bail!(REFUSED_KEY),
        status => bail!(failure(status, &text)),
    }
    read_models(&text).context("Anthropic\u{2019}s model list was not the JSON expected")
}

fn read_models(body: &str) -> Option<Vec<Model>> {
    let list: Value = serde_json::from_str(body).ok()?;
    let models = list["data"]
        .as_array()?
        .iter()
        .filter_map(|m| {
            let slug = m["id"].as_str()?.to_string();
            let name = m["display_name"].as_str().filter(|n| !n.trim().is_empty()).unwrap_or(&slug).to_string();
            Some(Model { slug, name, description: None })
        })
        .collect();
    Some(models)
}

fn headers(key: &str, json_body: bool) -> Vec<(String, String)> {
    let mut headers = vec![
        ("x-api-key".into(), key.to_string()),
        ("anthropic-version".into(), VERSION.into()),
        ("Accept".into(), "application/json".into()),
    ];
    if json_body {
        headers.push(("Content-Type".into(), "application/json".into()));
    }
    headers
}

/// The key, from memory or else the keychain.
fn key() -> Result<Option<String>> {
    if let Some(key) = cache().lock().ok().and_then(|c| c.clone()) {
        return Ok(Some(key));
    }
    let key = secrets::load(Slot::Claude, KEYCHAIN_ACCOUNT)?.filter(|k| !k.trim().is_empty());
    if let (Some(key), Ok(mut cache)) = (&key, cache().lock()) {
        *cache = Some(key.clone());
    }
    Ok(key)
}

fn cache() -> &'static Mutex<Option<String>> {
    static CACHE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn preference() -> &'static Mutex<Option<String>> {
    static PREFERENCE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    PREFERENCE.get_or_init(Default::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_asks_for_little_effort_and_names_no_sampling() {
        let body = request_body("claude-opus-5-5", "Be brief.", "Hello");
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["system"], "Be brief.");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Hello");
        assert_eq!(body["output_config"]["effort"], "low");
        // Refused outright on this API version.
        assert!(body.get("temperature").is_none());
        // Turned off is a 400 on the current models.
        assert!(body.get("thinking").is_none());
        assert_eq!(body["fallbacks"], "default");
    }

    #[test]
    fn fallbacks_are_only_sent_to_models_that_take_them() {
        assert!(request_body("claude-haiku-5-5", "", "").get("fallbacks").is_none());
        assert!(request_body("claude-sonnet-5-5", "", "").get("fallbacks").is_some());
    }

    #[test]
    fn the_answer_is_the_text_without_the_thinking() {
        let body = r#"{"content":[{"type":"thinking","thinking":""},{"type":"text","text":"Hi "},{"type":"text","text":"there"}],"stop_reason":"end_turn"}"#;
        assert_eq!(text_of(body).unwrap(), "Hi there");
    }

    #[test]
    fn a_refusal_or_a_cut_off_answer_is_an_error_not_text() {
        let refused = r#"{"content":[],"stop_reason":"refusal"}"#;
        assert!(text_of(refused).unwrap_err().to_string().contains("declined"));
        let cut = r#"{"content":[{"type":"text","text":"Dear"}],"stop_reason":"max_tokens"}"#;
        assert!(text_of(cut).unwrap_err().to_string().contains("cut off"));
    }

    #[test]
    fn an_api_error_says_what_anthropic_said() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"model: not found"}}"#;
        assert_eq!(failure(400, body), "Claude couldn\u{2019}t do this: model: not found");
        assert!(failure(529, "").contains("busy"));
    }

    #[test]
    fn the_model_list_keeps_the_api_order_and_names() {
        let body = r#"{"data":[{"id":"claude-opus-5-5","display_name":"Claude Opus 5.5"},{"id":"claude-haiku-5-5","display_name":""}],"has_more":false}"#;
        let models = read_models(body).unwrap();
        assert_eq!(models[0], Model { slug: "claude-opus-5-5".into(), name: "Claude Opus 5.5".into(), description: None });
        assert_eq!(models[1].name, "claude-haiku-5-5", "a blank name falls back to the id");
    }

    #[test]
    fn a_key_is_shown_by_its_last_four_characters() {
        assert_eq!(Key::of("sk-ant-api03-abcdWXYZ").hint, "\u{2026}WXYZ");
    }
}
