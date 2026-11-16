//! Signing in with a ChatGPT account, and asking its model for text.
//!
//! OpenAI has no sign-in for other apps that lets a ChatGPT plan pay for their
//! requests. What there is, is the one its own Codex CLI uses, and this is that
//! flow: the same public client, the same fixed loopback redirect, the same
//! backend. It is unofficial — OpenAI can change or close it without notice —
//! and the Writing section of Settings says so rather than leaving it to be
//! found out.
//!
//! The shape is the Google flow's (see `oauth.rs`): PKCE through the browser,
//! a code exchanged for tokens, the refresh token into the keychain and the
//! access token kept only in memory. Two things differ. The redirect is a
//! fixed port, because it is the only one the client is registered with. And
//! OpenAI rotates refresh tokens: every refresh hands back a new one and
//! retires the old, so each is written back — keeping the first would sign the
//! person out the second time a token expired.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::encoding::{base64_decode, base64_url, random_bytes, sha256};
use crate::http;
use crate::secrets::{self, Slot};

const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_HOST: &str = "auth.openai.com";
const TOKEN_PATH: &str = "/oauth/token";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// The Codex CLI's public client. Public in the OAuth sense: it ships inside
/// every copy of Codex, has no secret, and PKCE is what protects the exchange.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPE: &str = "openid profile email offline_access";

/// The one redirect the client accepts. Not a port the OS picks, so a Codex
/// sign-in running at the same moment holds it — see [`sign_in`].
const REDIRECT_PORT: u16 = 1455;
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

const BACKEND: &str = "https://chatgpt.com/backend-api/codex";

/// The Codex version the model list is asked for. The backend leaves out
/// models that need a newer client than the one asking.
const CLIENT_VERSION: &str = "0.160.1";

/// How long a read of the model list is trusted. Long enough that it is not
/// asked for on every rewrite, short enough that a model OpenAI retires stops
/// being asked for within the hour rather than at the next launch.
const MODELS_TTL: Duration = Duration::from_secs(3600);

/// Sent with a model that wants its instructions inside the input rather
/// than beside it, as Codex sends it.
const RESPONSES_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

/// The keychain entry's account name. There is one ChatGPT sign-in per
/// install, not one per mail account.
const KEYCHAIN_ACCOUNT: &str = "chatgpt";

/// See `oauth::CONSENT_TIMEOUT`: long enough to sign in, short enough that an
/// abandoned attempt does not hold the port for the rest of the session.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(300);

/// Refreshed this long before it expires, so a request does not start with a
/// token that runs out before the reply does.
const EXPIRY_MARGIN: Duration = Duration::from_secs(120);

/// Longer than any rewrite of an email takes, shorter than someone will
/// watch "Rewriting…" before assuming it hung.
const REPLY_TIMEOUT: Duration = Duration::from_secs(90);

/// The reply is a stream of events, each wrapping a word or two in JSON, so
/// it runs to many times the length of the text in it.
const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;

const NOT_SIGNED_IN: &str = "Sign in with ChatGPT in Settings \u{2192} Writing to use the writing tools.";
const SIGNED_OUT: &str = "Your ChatGPT sign-in has expired. Sign in again in Settings \u{2192} Writing.";

/// Who is signed in, for Settings to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub email: Option<String>,
    /// `plus`, `pro`, `team` and so on, as OpenAI names it.
    pub plan: Option<String>,
}

/// What the keychain holds: the refresh token, and what the id token said,
/// so Settings can name the account without a network round trip.
#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    refresh_token: String,
    account_id: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    plan: Option<String>,
}

impl Stored {
    fn account(&self) -> Account {
        Account { email: self.email.clone(), plan: self.plan.clone() }
    }
}

/// A model the account offers, for Settings to list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub slug: String,
    /// What OpenAI calls it, e.g. "GPT-5.5".
    pub name: String,
    pub description: Option<String>,
}

/// The models the account offers, in the order OpenAI recommends them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offered {
    pub models: Vec<Model>,
    /// The one used when none is picked.
    pub default: Option<String>,
}

/// An access token and what it is for.
#[derive(Clone)]
struct Live {
    access_token: String,
    account_id: String,
    expires_at: Instant,
}

impl Live {
    fn is_fresh(&self) -> bool {
        Instant::now() + EXPIRY_MARGIN < self.expires_at
    }
}

/// The ChatGPT account this install is signed in to, if any. Reads the
/// keychain, so it belongs on a background thread.
pub fn signed_in() -> Result<Option<Account>> {
    Ok(load()?.map(|stored| stored.account()))
}

/// Forgets the sign-in, here and in the keychain.
pub fn sign_out() -> Result<()> {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut cache) = cache().lock() {
        *cache = None;
    }
    forget_models();
    secrets::delete(Slot::ChatGpt, KEYCHAIN_ACCOUNT)
}

/// Runs the browser sign-in and keeps what it yields.
///
/// `open_url` is the caller's, as in `oauth::authorize`.
pub async fn sign_in(open_url: impl FnOnce(&str)) -> Result<Account> {
    let listener = match smol::net::TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => bail!(
            "another app is signing in to ChatGPT right now (port {REDIRECT_PORT} is taken \
             \u{2014} often a Codex sign-in). Finish or close that one, then try again."
        ),
        Err(e) => return Err(e).context("opening a local port for the sign-in redirect"),
    };

    let verifier = base64_url(&random_bytes(32)?);
    let challenge = base64_url(&sha256(verifier.as_bytes()));
    let state = base64_url(&random_bytes(16)?);

    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPE),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
        // Puts the ChatGPT account ID in the id token, which the backend
        // wants on every request.
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
    ];
    open_url(&format!("{AUTHORIZE_URL}?{}", http::encode_form(&params)));

    let redirect = crate::oauth::wait_for_redirect(&listener, &state, "ChatGPT");
    let code = smol::future::or(redirect, async {
        smol::Timer::after(CONSENT_TIMEOUT).await;
        bail!("timed out waiting for the browser \u{2014} the sign-in was never completed")
    })
    .await?;
    drop(listener);

    let response = http::post_form(
        TOKEN_HOST,
        TOKEN_PATH,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", &verifier),
        ],
    )
    .await
    .context("talking to OpenAI's sign-in server")?;
    if !response.is_success() {
        bail!("OpenAI refused the sign-in: {}", error_text(response.status, &response.body));
    }
    let tokens: TokenResponse =
        serde_json::from_str(&response.body).context("OpenAI's token response was not the JSON expected")?;

    let claims = tokens.id_token.as_deref().map(Claims::of).unwrap_or_default();
    let account_id = claims
        .account_id
        .or_else(|| Claims::of(&tokens.access_token).account_id)
        .context("OpenAI did not say which ChatGPT account this is")?;
    let refresh_token = tokens
        .refresh_token
        .clone()
        .filter(|t| !t.is_empty())
        .context("OpenAI returned no refresh token, so the sign-in would not last")?;

    let stored = Stored { refresh_token, account_id: account_id.clone(), email: claims.email, plan: claims.plan };
    GENERATION.fetch_add(1, Ordering::SeqCst);
    save(&stored)?;
    // Another account may offer other models.
    forget_models();
    if let Ok(mut cache) = cache().lock() {
        *cache = Some(Live { access_token: tokens.access_token.clone(), account_id, expires_at: tokens.expiry() });
    }
    Ok(stored.account())
}

/// Asks the model to follow `instructions` on `prompt`, and returns its text.
pub async fn respond(instructions: &str, prompt: &str) -> Result<String> {
    let work = async {
        let live = session().await?;
        let target = target(&live).await?;
        let body = request_body(&target, instructions, prompt);

        let mut headers = backend_headers(&live, "text/event-stream", true);
        if target.lite {
            headers.push((RESPONSES_LITE_HEADER.into(), "true".into()));
        }
        let reply = http::request(
            "POST",
            &format!("{BACKEND}/responses"),
            &headers,
            Some(body.to_string().as_bytes()),
            MAX_REPLY_BYTES,
        )
        .await
        .context("talking to ChatGPT")?;

        let text = String::from_utf8_lossy(&reply.body);
        match reply.status {
            200..=299 => text_of_stream(&text),
            401 => {
                // Revoked elsewhere, or signed out on the web: the cached token
                // is no good, and the next try should find that out afresh.
                forget_cached();
                bail!(SIGNED_OUT)
            }
            status => {
                // Often a model that has just been retired: the next try reads
                // the list again rather than asking for it for the next hour.
                forget_models();
                bail!("ChatGPT couldn\u{2019}t do this: {}", error_text(status, &text))
            }
        }
    };

    smol::future::or(work, async {
        smol::Timer::after(REPLY_TIMEOUT).await;
        bail!("ChatGPT took too long to answer. Try again in a moment.")
    })
    .await
}

/// A usable access token, refreshing when the cached one is about to run out.
///
/// Refreshes one at a time. Two composers refreshing together would each
/// spend the same refresh token, and with rotation the second is refused —
/// which reads as being signed out.
async fn session() -> Result<Live> {
    if let Some(live) = cached() {
        return Ok(live);
    }
    let _refreshing = refresh_lock().lock().await;
    // Another refresh may have finished while this one waited.
    if let Some(live) = cached() {
        return Ok(live);
    }

    let generation = GENERATION.load(Ordering::SeqCst);
    let stored = load()?.ok_or_else(|| anyhow!(NOT_SIGNED_IN))?;

    let body = json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": stored.refresh_token,
        "scope": "openid profile email",
    });
    let reply = http::request(
        "POST",
        TOKEN_URL,
        &[("Content-Type".into(), "application/json".into()), ("Accept".into(), "application/json".into())],
        Some(body.to_string().as_bytes()),
        64 * 1024,
    )
    .await
    .context("talking to OpenAI's sign-in server")?;
    let text = String::from_utf8_lossy(&reply.body);
    if !(200..300).contains(&reply.status) {
        // A revoked, expired or already-spent refresh token. OpenAI names the
        // last kind `refresh_token_reused` and friends.
        let refused = text.contains("invalid_grant") || text.contains("refresh_token_");
        if (400..500).contains(&reply.status) && refused {
            bail!(SIGNED_OUT);
        }
        bail!("couldn\u{2019}t renew the ChatGPT sign-in: {}", error_text(reply.status, &text));
    }
    let tokens: TokenResponse =
        serde_json::from_str(&text).context("OpenAI's token response was not the JSON expected")?;

    // Signed out, or in again, while this was on the wire: what it got
    // belongs to a sign-in that no longer exists.
    if GENERATION.load(Ordering::SeqCst) != generation {
        bail!(NOT_SIGNED_IN);
    }

    let claims = tokens.id_token.as_deref().map(Claims::of).unwrap_or_default();
    let rotated = tokens.refresh_token.clone().filter(|t| !t.is_empty() && *t != stored.refresh_token);
    if let Some(refresh_token) = rotated {
        let updated = Stored {
            refresh_token,
            account_id: claims.account_id.clone().unwrap_or_else(|| stored.account_id.clone()),
            email: claims.email.clone().or(stored.email.clone()),
            plan: claims.plan.clone().or(stored.plan.clone()),
        };
        // The old token is already spent; losing the new one signs the
        // person out, so a failure here is worth failing the request over.
        save(&updated).context("saving the renewed ChatGPT sign-in")?;
    }

    let live = Live {
        access_token: tokens.access_token.clone(),
        account_id: claims.account_id.unwrap_or(stored.account_id),
        expires_at: tokens.expiry(),
    };
    if let Ok(mut cache) = cache().lock() {
        *cache = Some(live.clone());
    }
    Ok(live)
}

/// Uses `model` from now on, or the account's default when `None`. Settings
/// calls this at launch and whenever the choice changes. A model the account
/// stops offering is passed over for the default rather than failing.
pub fn set_model(model: Option<String>) {
    if let Ok(mut preference) = preference().lock() {
        *preference = model.filter(|m| !m.trim().is_empty());
    }
}

/// The models the account offers, read afresh, for Settings to list.
pub async fn models() -> Result<Offered> {
    let live = session().await?;
    forget_models();
    let models = catalog(&live).await?;
    Ok(Offered {
        models: offered(&models).map(Listed::model).collect(),
        default: default_of(&models).map(|m| m.slug.clone()),
    })
}

/// What a request is sent to, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    slug: String,
    lite: bool,
    effort: Option<String>,
}

impl Target {
    /// A model named outright that the list says nothing about: sent the
    /// ordinary way.
    fn plain(slug: &str) -> Self {
        Target { slug: slug.to_string(), lite: false, effort: Some("low".into()) }
    }
}

/// The model to ask: `SPOOL_CHATGPT_MODEL` when set, else the one picked in
/// Settings while the account still offers it, else the account's default.
///
/// There is no built-in model to fall back on. Any name written in here
/// goes stale the day OpenAI retires it, and a stale name is a confusing
/// error where a missing list is an honest one.
async fn target(live: &Live) -> Result<Target> {
    let forced = std::env::var("SPOOL_CHATGPT_MODEL").ok().map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
    let picked = preference().lock().ok().and_then(|p| p.clone());

    match catalog(live).await {
        Ok(models) => choose(&models, forced.as_deref(), picked.as_deref())
            .context("ChatGPT didn\u{2019}t offer any models to write with."),
        // Without the list there is no default, but a model named outright
        // is still worth a try.
        Err(e) => match forced.or(picked) {
            Some(slug) => {
                log::warn!("{e:#}; trying {slug} anyway");
                Ok(Target::plain(&slug))
            }
            None => Err(e),
        },
    }
}

fn choose(models: &[Listed], forced: Option<&str>, picked: Option<&str>) -> Option<Target> {
    if let Some(forced) = forced {
        let listed = models.iter().find(|m| m.slug == forced);
        return Some(listed.map(Listed::target).unwrap_or_else(|| Target::plain(forced)));
    }
    if let Some(picked) = picked {
        match offered(models).find(|m| m.slug == picked) {
            Some(model) => return Some(model.target()),
            None => log::warn!("{picked} is no longer offered; the writing tools use the default"),
        }
    }
    default_of(models).map(Listed::target)
}

/// The account's model list, read at most once per [`MODELS_TTL`].
async fn catalog(live: &Live) -> Result<Arc<Vec<Listed>>> {
    if let Some(models) = cached_models() {
        return Ok(models);
    }
    let reply = http::request(
        "GET",
        &format!("{BACKEND}/models?client_version={CLIENT_VERSION}"),
        &backend_headers(live, "application/json", false),
        None,
        1024 * 1024,
    )
    .await
    .context("asking ChatGPT which models it offers")?;
    match reply.status {
        200..=299 => {}
        401 => {
            forget_cached();
            bail!(SIGNED_OUT)
        }
        status => bail!(
            "ChatGPT wouldn\u{2019}t list its models: {}",
            error_text(status, &String::from_utf8_lossy(&reply.body))
        ),
    }
    let models = read_models(&reply.body).context("ChatGPT\u{2019}s model list was not the JSON expected")?;
    if let Some(default) = default_of(&models) {
        log::info!("ChatGPT offers {} models; the default is {}", offered(&models).count(), default.slug);
    }

    let models = Arc::new(models);
    if let Ok(mut cache) = models_cache().lock() {
        *cache = Some((Instant::now(), models.clone()));
    }
    Ok(models)
}

/// One entry of the backend's model list: what Settings shows, and what
/// changes how a request is made. The rest is for Codex's own tools.
#[derive(Debug, Clone, Deserialize)]
struct Listed {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// `list` for the ones offered in a picker. `hide` and `none` are not.
    #[serde(default)]
    visibility: Option<String>,
    /// Lowest first.
    #[serde(default)]
    priority: Option<i64>,
    /// Set on models made for one kind of work, such as security research.
    #[serde(default)]
    model_specialty: Option<String>,
    /// Wants its instructions inside the input; see [`request_body`].
    #[serde(default)]
    use_responses_lite: bool,
    /// Objects with an `effort`. Kept loose so an odd one costs that level
    /// rather than the whole model.
    #[serde(default)]
    supported_reasoning_levels: Vec<Value>,
}

impl Listed {
    fn is_offered(&self) -> bool {
        self.visibility.as_deref().is_none_or(|v| v == "list")
    }

    fn model(&self) -> Model {
        let text = |t: &Option<String>| t.clone().filter(|t| !t.trim().is_empty());
        Model {
            slug: self.slug.clone(),
            name: text(&self.display_name).unwrap_or_else(|| self.slug.clone()),
            description: text(&self.description),
        }
    }

    fn target(&self) -> Target {
        let levels: Vec<&str> =
            self.supported_reasoning_levels.iter().filter_map(|l| l["effort"].as_str()).collect();
        Target { slug: self.slug.clone(), lite: self.use_responses_lite, effort: effort(&levels) }
    }
}

/// Rewording an email needs little thought, and the default effort takes
/// several times as long to say the same thing: `low`, or the nearest to it
/// the model has. A model that lists none is sent `low`.
fn effort(levels: &[&str]) -> Option<String> {
    const NEAREST_LOW: [&str; 4] = ["low", "minimal", "none", "medium"];
    if levels.is_empty() {
        return Some("low".into());
    }
    let effort = NEAREST_LOW.into_iter().find(|e| levels.contains(e)).or(levels.first().copied());
    effort.map(str::to_string)
}

/// The backend's model list, in the order it recommends them.
///
/// An entry that does not parse is skipped rather than losing the list: the
/// format is Codex's, and it changes most releases.
///
/// `supported_in_api` is not read. It says whether a model can be used with
/// an API key, and Codex ignores it for a ChatGPT sign-in. The newest models
/// are often ChatGPT-only, and filtering on it left only retired ones.
fn read_models(body: &[u8]) -> Option<Vec<Listed>> {
    #[derive(Deserialize)]
    struct List {
        models: Vec<Value>,
    }
    let list: List = serde_json::from_slice(body).ok()?;
    let mut models: Vec<Listed> = list.models.into_iter().filter_map(|m| serde_json::from_value(m).ok()).collect();
    // Stable, so equal priorities keep the backend's order.
    models.sort_by_key(|m| m.priority.unwrap_or(i64::MAX));
    Some(models)
}

fn offered(models: &[Listed]) -> impl Iterator<Item = &Listed> {
    models.iter().filter(|m| m.is_offered())
}

/// The model used when none is picked: the first one offered for general
/// use, passing over the ones tuned for writing code or for a specialty.
fn default_of(models: &[Listed]) -> Option<&Listed> {
    offered(models)
        .find(|m| !m.slug.contains("codex") && m.model_specialty.is_none())
        .or_else(|| offered(models).next())
        .or_else(|| models.first())
}

/// The body of a `respond` request.
///
/// A "Responses Lite" model takes its instructions as a developer message
/// ahead of the prompt, with no `instructions` field at all, and its
/// reasoning set to span the whole input. That is how Codex sends it.
fn request_body(target: &Target, instructions: &str, prompt: &str) -> Value {
    let message = |role: &str, text: &str| {
        json!({ "type": "message", "role": role, "content": [{ "type": "input_text", "text": text }] })
    };
    let mut input = Vec::new();
    if target.lite {
        input.push(message("developer", instructions));
    }
    input.push(message("user", prompt));

    let mut reasoning = json!({});
    if let Some(effort) = &target.effort {
        reasoning["effort"] = json!(effort);
    }
    if target.lite {
        reasoning["context"] = json!("all_turns");
    }

    let mut body = json!({
        "model": target.slug,
        "input": input,
        "reasoning": reasoning,
        // There are no tools to call. Codex sends it with every request,
        // and Responses Lite refuses the request without it.
        "parallel_tool_calls": false,
        // Nothing kept on OpenAI's side past answering. The backend
        // also refuses anything else from this client.
        "store": false,
        "stream": true,
    });
    if !target.lite {
        body["instructions"] = json!(instructions);
    }
    body
}

fn backend_headers(live: &Live, accept: &str, json_body: bool) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Authorization".into(), format!("Bearer {}", live.access_token)),
        ("chatgpt-account-id".into(), live.account_id.clone()),
        ("OpenAI-Beta".into(), "responses=experimental".into()),
        ("originator".into(), "codex_cli_rs".into()),
        ("Accept".into(), accept.into()),
    ];
    if json_body {
        headers.push(("Content-Type".into(), "application/json".into()));
    }
    headers
}

/// The text of a streamed reply.
///
/// The words arrive as `response.output_text.delta` events. The finished
/// text is also in `response.output_text.done`, and the whole response in
/// `response.completed`; those are read only when no deltas came, so a
/// reply is never counted twice.
fn text_of_stream(stream: &str) -> Result<String> {
    let mut deltas = String::new();
    let mut done = String::new();
    let mut completed = String::new();

    for event in stream.replace("\r\n", "\n").split("\n\n") {
        let data: String = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(&data) else { continue };

        match event["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => deltas.push_str(event["delta"].as_str().unwrap_or_default()),
            "response.output_text.done" => done.push_str(event["text"].as_str().unwrap_or_default()),
            "response.completed" => completed = output_text(&event["response"]),
            "response.failed" | "response.incomplete" | "error" => {
                let error = &event["response"]["error"];
                let reason = error["message"]
                    .as_str()
                    .or(event["message"].as_str())
                    .or(event["error"]["message"].as_str())
                    .or(event["response"]["incomplete_details"]["reason"].as_str())
                    .unwrap_or("the reply was cut off");
                bail!("ChatGPT couldn\u{2019}t do this: {reason}");
            }
            _ => {}
        }
    }

    let text = [deltas, done, completed].into_iter().find(|t| !t.trim().is_empty());
    text.ok_or_else(|| anyhow!("ChatGPT sent back an empty reply. Try again."))
}

/// The text of a finished response's output messages.
fn output_text(response: &Value) -> String {
    let mut text = String::new();
    for item in response["output"].as_array().into_iter().flatten() {
        for part in item["content"].as_array().into_iter().flatten() {
            if part["type"] == "output_text" {
                text.push_str(part["text"].as_str().unwrap_or_default());
            }
        }
    }
    text
}

/// A refusal from either server, as a sentence. Both put the reason in the
/// body, in one of a few shapes.
fn error_text(status: u16, body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let reason = value["detail"]
            .as_str()
            .or(value["detail"]["message"].as_str())
            .or(value["error"]["message"].as_str())
            .or(value["error_description"].as_str())
            .or(value["error"].as_str())
            .or(value["message"].as_str());
        if let Some(reason) = reason.filter(|r| !r.trim().is_empty()) {
            return reason.trim().to_string();
        }
    }
    let body = body.trim();
    match body.is_empty() {
        true => format!("HTTP {status}"),
        false => format!("HTTP {status}: {}", body.chars().take(200).collect::<String>()),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl TokenResponse {
    /// When the access token runs out: as the response says, else as the
    /// token itself says, else in an hour.
    fn expiry(&self) -> Instant {
        let seconds = self.expires_in.or_else(|| Claims::of(&self.access_token).expires_in).unwrap_or(3600);
        Instant::now() + Duration::from_secs(seconds)
    }
}

/// What is read out of an OpenAI token.
///
/// Unverified, as in `oauth::email_from_id_token`, and for the same reason:
/// it came straight from OpenAI over TLS in answer to this request. It names
/// the account; it decides nothing about access.
#[derive(Debug, Default, PartialEq, Eq)]
struct Claims {
    account_id: Option<String>,
    email: Option<String>,
    plan: Option<String>,
    /// Seconds from now until `exp`.
    expires_in: Option<u64>,
}

impl Claims {
    fn of(token: &str) -> Self {
        let Some(json) = token.split('.').nth(1).and_then(base64_decode) else {
            return Self::default();
        };
        let Ok(claims) = serde_json::from_slice::<Value>(&json) else {
            return Self::default();
        };
        let auth = &claims["https://api.openai.com/auth"];
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();

        Self {
            account_id: text(&auth["chatgpt_account_id"]).or_else(|| text(&claims["chatgpt_account_id"])),
            email: text(&claims["email"]).or_else(|| text(&claims["https://api.openai.com/profile"]["email"])),
            plan: text(&auth["chatgpt_plan_type"]),
            expires_in: claims["exp"].as_u64().map(|exp| exp.saturating_sub(now)),
        }
    }
}

fn load() -> Result<Option<Stored>> {
    let Some(json) = secrets::load(Slot::ChatGpt, KEYCHAIN_ACCOUNT)? else {
        return Ok(None);
    };
    // Unreadable is treated as signed out rather than as an error on every
    // request: signing in again overwrites it.
    Ok(serde_json::from_str(&json).map_err(|e| log::warn!("unreadable ChatGPT sign-in: {e}")).ok())
}

fn save(stored: &Stored) -> Result<()> {
    secrets::store(Slot::ChatGpt, KEYCHAIN_ACCOUNT, &serde_json::to_string(stored)?)
}

/// Bumped by signing in and out, so a refresh that was already under way
/// cannot bring back a sign-in that has since been replaced or removed.
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn cache() -> &'static Mutex<Option<Live>> {
    static CACHE: OnceLock<Mutex<Option<Live>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn cached() -> Option<Live> {
    cache().lock().ok()?.clone().filter(Live::is_fresh)
}

fn forget_cached() {
    if let Ok(mut cache) = cache().lock() {
        *cache = None;
    }
}

/// The model list, and when it was read.
type ModelsCache = Mutex<Option<(Instant, Arc<Vec<Listed>>)>>;

fn models_cache() -> &'static ModelsCache {
    static CACHE: OnceLock<ModelsCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn cached_models() -> Option<Arc<Vec<Listed>>> {
    let cache = models_cache().lock().ok()?;
    let (read_at, models) = cache.as_ref()?;
    (read_at.elapsed() < MODELS_TTL).then(|| models.clone())
}

fn forget_models() {
    if let Ok(mut cache) = models_cache().lock() {
        *cache = None;
    }
}

/// The model picked in Settings; see [`set_model`].
fn preference() -> &'static Mutex<Option<String>> {
    static PREFERENCE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    PREFERENCE.get_or_init(Default::default)
}

fn refresh_lock() -> &'static smol::lock::Mutex<()> {
    static LOCK: OnceLock<smol::lock::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(Default::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(claims: Value) -> String {
        format!("eyJhbGciOiJSUzI1NiJ9.{}.signature", base64_url(claims.to_string().as_bytes()))
    }

    #[test]
    fn the_account_comes_out_of_the_id_token() {
        let id = token(json!({
            "email": "me@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-1", "chatgpt_plan_type": "plus" },
        }));
        let claims = Claims::of(&id);
        assert_eq!(claims.account_id.as_deref(), Some("acct-1"));
        assert_eq!(claims.email.as_deref(), Some("me@example.com"));
        assert_eq!(claims.plan.as_deref(), Some("plus"));
    }

    #[test]
    fn a_malformed_token_has_no_claims_rather_than_panicking() {
        assert_eq!(Claims::of(""), Claims::default());
        assert_eq!(Claims::of("a.!!!.c"), Claims::default());
        assert_eq!(Claims::of(&format!("a.{}.c", base64_url(b"[]"))), Claims::default());
    }

    #[test]
    fn the_streamed_words_are_put_together_once() {
        let stream = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
            event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi \"}\n\n\
            event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Ana,\\nthanks.\"}\n\n\
            event: response.output_text.done\ndata: {\"type\":\"response.output_text.done\",\"text\":\"Hi Ana,\\nthanks.\"}\n\n\
            event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hi Ana,\\nthanks.\"}]}]}}\n\n";
        assert_eq!(text_of_stream(stream).unwrap(), "Hi Ana,\nthanks.");
    }

    #[test]
    fn a_reply_with_no_deltas_is_read_from_the_finished_response() {
        let stream = "data: {\"type\":\"response.completed\",\"response\":{\"output\":[\
            {\"type\":\"reasoning\"},\
            {\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Done.\"}]}]}}\r\n\r\n";
        assert_eq!(text_of_stream(stream).unwrap(), "Done.");
    }

    #[test]
    fn a_failed_reply_says_why() {
        let stream = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"You've hit your usage limit.\"}}}\n\n";
        let error = text_of_stream(stream).unwrap_err().to_string();
        assert!(error.contains("usage limit"), "{error}");
        assert!(text_of_stream("").is_err(), "nothing is not a reply");
    }

    fn listed(models: Value) -> Vec<Listed> {
        read_models(json!({ "models": models }).to_string().as_bytes()).unwrap()
    }

    #[test]
    fn the_default_is_the_first_general_model_by_priority() {
        let models = listed(json!([
            { "slug": "old", "visibility": "list", "priority": 9 },
            { "slug": "hidden", "visibility": "hide", "priority": 0 },
            { "slug": "gpt-9-codex", "visibility": "list", "priority": 1 },
            { "slug": "gpt-9-cyber", "visibility": "list", "priority": 2, "model_specialty": "cyber" },
            // ChatGPT-only, which is not a reason to pass it over here.
            { "slug": "gpt-9", "visibility": "list", "priority": 3, "supported_in_api": false },
        ]));
        assert_eq!(default_of(&models).map(|m| m.slug.as_str()), Some("gpt-9"));
        let offered: Vec<_> = offered(&models).map(|m| m.slug.as_str()).collect();
        assert_eq!(offered, ["gpt-9-codex", "gpt-9-cyber", "gpt-9", "old"]);

        let only_codex = listed(json!([{ "slug": "gpt-9-codex" }]));
        assert_eq!(default_of(&only_codex).map(|m| m.slug.as_str()), Some("gpt-9-codex"));
        assert!(read_models(b"not json").is_none());
    }

    #[test]
    fn an_entry_that_does_not_parse_costs_only_itself() {
        let models = listed(json!([{ "display_name": "No slug" }, { "slug": "gpt-9", "display_name": "GPT-9" }]));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model(), Model { slug: "gpt-9".into(), name: "GPT-9".into(), description: None });
    }

    #[test]
    fn a_picked_model_is_used_while_it_is_offered() {
        let models = listed(json!([
            { "slug": "gpt-9", "priority": 1 },
            { "slug": "gpt-8", "priority": 2 },
            { "slug": "gpt-7", "priority": 3, "visibility": "hide" },
        ]));
        let slug = |t: Option<Target>| t.map(|t| t.slug);
        assert_eq!(slug(choose(&models, None, Some("gpt-8"))).as_deref(), Some("gpt-8"));
        assert_eq!(slug(choose(&models, None, Some("gpt-5.2"))).as_deref(), Some("gpt-9"), "retired");
        assert_eq!(slug(choose(&models, None, Some("gpt-7"))).as_deref(), Some("gpt-9"), "no longer offered");
        assert_eq!(slug(choose(&models, Some("gpt-7"), Some("gpt-8"))).as_deref(), Some("gpt-7"), "forced");
        assert_eq!(choose(&models, Some("unlisted"), None), Some(Target::plain("unlisted")));
        assert_eq!(choose(&[], None, None), None);
    }

    #[test]
    fn the_effort_is_low_or_the_nearest_the_model_has() {
        assert_eq!(effort(&[]).as_deref(), Some("low"));
        assert_eq!(effort(&["medium", "low", "high"]).as_deref(), Some("low"));
        assert_eq!(effort(&["high", "medium", "minimal"]).as_deref(), Some("minimal"));
        assert_eq!(effort(&["high", "xhigh"]).as_deref(), Some("high"));

        let models = listed(json!([{ "slug": "gpt-9", "supported_reasoning_levels": [
            { "effort": "medium", "description": "Balanced" }, { "effort": "high", "description": "Deep" },
        ]}]));
        assert_eq!(models[0].target().effort.as_deref(), Some("medium"));
    }

    #[test]
    fn a_lite_model_gets_its_instructions_in_the_input() {
        let ordinary = request_body(&Target::plain("gpt-9"), "Be brief.", "Hello");
        assert_eq!(ordinary["instructions"], "Be brief.");
        assert_eq!(ordinary["input"].as_array().unwrap().len(), 1);
        assert_eq!(ordinary["reasoning"], json!({ "effort": "low" }));

        let lite = Target { slug: "gpt-10".into(), lite: true, effort: Some("low".into()) };
        let body = request_body(&lite, "Be brief.", "Hello");
        assert!(body.get("instructions").is_none(), "absent, not empty");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"][0]["text"], "Be brief.");
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"][1]["content"][0]["text"], "Hello");
        assert_eq!(body["reasoning"], json!({ "effort": "low", "context": "all_turns" }));
        assert_eq!(body["store"], false);
    }

    #[test]
    fn a_refusal_is_the_servers_reason() {
        assert_eq!(error_text(429, r#"{"detail":"Rate limited"}"#), "Rate limited");
        assert_eq!(error_text(400, r#"{"error":{"message":"Bad model"}}"#), "Bad model");
        assert_eq!(
            error_text(400, r#"{"error":"invalid_grant","error_description":"Token reused"}"#),
            "Token reused"
        );
        assert_eq!(error_text(502, ""), "HTTP 502");
        assert!(error_text(502, "<html>bad gateway</html>").contains("bad gateway"));
    }
}
