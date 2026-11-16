//! Google's OAuth 2.0 flow for desktop apps.
//!
//! Gmail no longer accepts a password over IMAP for most accounts, so the only
//! way in is a bearer token. The shape of getting one:
//!
//! 1. Open a loopback listener on a port the OS picks.
//! 2. Send the user's browser to Google with that port as the redirect.
//! 3. Google redirects back to the listener with a one-time code.
//! 4. Exchange the code for an access token and a refresh token.
//!
//! The refresh token is the durable credential and goes straight to the
//! keychain; the access token lives about an hour and is kept only in memory,
//! because writing an hour-long secret to disk buys nothing.
//!
//! PKCE is used even though the flow has a client secret. A desktop client's
//! secret is not actually secret — it ships inside the app — so the proof key
//! is what really binds the redirect to this process.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures::AsyncReadExt;
use futures::io::AsyncWriteExt;
use serde::Deserialize;

use crate::encoding::{base64_url, base64_decode, random_bytes, sha256};
use crate::http;

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_HOST: &str = "oauth2.googleapis.com";
const TOKEN_PATH: &str = "/token";

/// `https://mail.google.com/` is the scope that grants IMAP and SMTP; the
/// narrower gmail.* scopes only reach the REST API. `email` comes along so the
/// returned identity can fill in the address rather than asking for it twice.
const SCOPE: &str = "https://mail.google.com/ email";

/// How long to wait for the browser round trip before giving up.
///
/// Long enough to find the right Google account and read a consent screen,
/// short enough that an abandoned attempt does not leave a socket and a task
/// alive for the rest of the session.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(300);

/// An access token is refreshed this long before it actually expires, so a
/// sync that starts just under the wire does not fail mid-flight.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// The OAuth client this build of Spool is, as registered in Google Cloud.
///
/// Empty in the source. Released builds get theirs from
/// `SPOOL_GOOGLE_CLIENT_ID` and `SPOOL_GOOGLE_CLIENT_SECRET` at compile time
/// (`scripts/package.sh` refuses to build a release without them), so the
/// official client is not something every fork inherits.
///
/// **To give your own build one:** create an OAuth client of type *Desktop
/// app* under APIs & Services in a Google Cloud project with the Gmail API
/// enabled, then either export those two variables before `cargo build` or
/// paste the values in here — that is the whole configuration, and afterwards
/// signing in is one button for the user.
///
/// A desktop client's secret is not a secret in the usual sense. Google says
/// so outright: it ships inside every copy of the application, so it cannot be
/// kept, and the OAuth spec calls such a client "public" for that reason. What
/// actually protects the exchange is PKCE plus the loopback redirect, both of
/// which this flow uses. Someone who lifts these strings can impersonate the
/// *app* on a consent screen; they cannot touch anyone's mail.
const BUILT_IN_CLIENT_ID: &str = "";
const BUILT_IN_CLIENT_SECRET: &str = "";

/// The credentials identifying *the app* to Google, as opposed to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl ClientCredentials {
    /// The client this build signs in with.
    ///
    /// Three places, in order: the environment at run time, the environment at
    /// build time, then the constants above. The run-time variables let
    /// someone use their own Google project without rebuilding; the build-time
    /// ones let a release pipeline inject credentials without committing them.
    pub fn bundled() -> Result<Self> {
        let client_id = resolve("SPOOL_GOOGLE_CLIENT_ID", BUILT_IN_CLIENT_ID);
        let client_secret = resolve("SPOOL_GOOGLE_CLIENT_SECRET", BUILT_IN_CLIENT_SECRET);

        match (client_id, client_secret) {
            (Some(client_id), Some(client_secret)) => Ok(Self { client_id, client_secret }),
            // Named precisely, because the person reading this is a developer
            // holding a build that cannot sign in, not a user who typed
            // something wrong.
            _ => bail!(
                "this build has no Google OAuth client — set BUILT_IN_CLIENT_ID and \
                 BUILT_IN_CLIENT_SECRET in crates/net/src/oauth.rs, or set \
                 SPOOL_GOOGLE_CLIENT_ID and SPOOL_GOOGLE_CLIENT_SECRET"
            ),
        }
    }

    /// Whether signing in with Google can work at all here.
    ///
    /// Lets the pane say so up front rather than after someone presses a
    /// button and waits for a browser that never opens.
    pub fn is_available() -> bool {
        Self::bundled().is_ok()
    }
}

/// A credential from the environment at run time, then at build time, then the
/// compiled-in fallback. Blank counts as absent everywhere.
fn resolve(variable: &str, built_in: &str) -> Option<String> {
    let non_empty = |value: &str| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    };

    std::env::var(variable).ok().as_deref().and_then(non_empty)
        .or_else(|| match variable {
            "SPOOL_GOOGLE_CLIENT_ID" => option_env!("SPOOL_GOOGLE_CLIENT_ID"),
            "SPOOL_GOOGLE_CLIENT_SECRET" => option_env!("SPOOL_GOOGLE_CLIENT_SECRET"),
            _ => None,
        }
        .and_then(non_empty))
        .or_else(|| non_empty(built_in))
}

/// What a completed sign-in yields.
pub struct Grant {
    /// The credential worth keeping. Google returns it only on the first
    /// consent, which is why the flow forces the consent screen.
    pub refresh_token: String,
    pub access_token: AccessToken,
    /// The address Google says this is, from the id token. Used to fill the
    /// form in, and to catch signing in as the wrong account.
    pub email: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AccessToken {
    pub token: String,
    expires_at: Instant,
}

impl AccessToken {
    fn is_fresh(&self) -> bool {
        Instant::now() + EXPIRY_MARGIN < self.expires_at
    }
}

/// The SASL initial response for `AUTHENTICATE XOAUTH2`, per Google's spec.
///
/// The `\x01` separators are part of the format, not a typo. The result is
/// base64-encoded by whatever sends it, so this returns the raw bytes.
pub fn xoauth2(user: &str, access_token: &str) -> String {
    format!("user={user}\x01auth=Bearer {access_token}\x01\x01")
}

/// Runs the whole browser flow and returns the resulting tokens.
///
/// `open_url` is a callback rather than a hardcoded `open(1)` so the caller
/// decides how the browser is launched, and so a test can drive the flow
/// without one.
pub async fn authorize(
    client: &ClientCredentials,
    login_hint: Option<&str>,
    open_url: impl FnOnce(&str),
) -> Result<Grant> {
    if client.client_id.trim().is_empty() {
        bail!(
            "this build has no Google OAuth client ID — see BUILT_IN_CLIENT_ID in \
             crates/net/src/oauth.rs"
        );
    }

    // Bind before building the URL: the redirect has to name the port, and
    // only the OS knows which one is free.
    let listener = smol::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("opening a local port for the sign-in redirect")?;
    let port = listener
        .local_addr()
        .context("reading the local redirect port")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let verifier = base64_url(&random_bytes(32)?);
    let challenge = base64_url(&sha256(verifier.as_bytes()));
    let state = base64_url(&random_bytes(16)?);

    let mut params = vec![
        ("client_id", client.client_id.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
        // Without `offline` Google returns no refresh token, and without
        // `consent` it returns one only the very first time — so a user who
        // re-added an account would get a session that dies in an hour.
        ("access_type", "offline"),
        ("prompt", "consent"),
    ];
    if let Some(hint) = login_hint {
        params.push(("login_hint", hint));
    }

    let url = format!("{AUTH_ENDPOINT}?{}", http::encode_form(&params));
    open_url(&url);

    let code = wait_for_redirect(&listener, &state, "Google").await?;
    exchange(
        client,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &verifier),
        ],
    )
    .await
}

/// Trades a refresh token for a fresh access token.
pub async fn refresh(client: &ClientCredentials, refresh_token: &str) -> Result<AccessToken> {
    let grant = exchange(
        client,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh_token)],
    )
    .await?;
    Ok(grant.access_token)
}

/// The access token for an account, refreshing only when the cached one is
/// about to expire.
///
/// A sync runs every few minutes and a token lasts an hour, so without this
/// nearly every sync would open a second connection to Google before it opened
/// the first one to the mail server.
pub async fn access_token(
    account: &str,
    client: &ClientCredentials,
    refresh_token: &str,
) -> Result<String> {
    if let Some(cached) = cache().lock().ok().and_then(|c| c.get(account).cloned())
        && cached.is_fresh()
    {
        return Ok(cached.token);
    }

    let token = refresh(client, refresh_token)
        .await
        .with_context(|| format!("refreshing the Google token for {account}"))?;

    if let Ok(mut cache) = cache().lock() {
        cache.insert(account.to_string(), token.clone());
    }
    Ok(token.token)
}

/// Drops a cached token, so the next sync re-reads whatever was just stored.
pub fn forget(account: &str) {
    if let Ok(mut cache) = cache().lock() {
        cache.remove(account);
    }
}

fn cache() -> &'static Mutex<HashMap<String, AccessToken>> {
    static CACHE: OnceLock<Mutex<HashMap<String, AccessToken>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Google's token response. `refresh_token` is absent on a refresh, and
/// `id_token` is absent unless the `email` scope was granted.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default = "default_expiry")]
    expires_in: u64,
}

fn default_expiry() -> u64 {
    3600
}

/// The error shape Google returns with a 4xx.
#[derive(Deserialize)]
struct TokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

async fn exchange(client: &ClientCredentials, extra: &[(&str, &str)]) -> Result<Grant> {
    let mut fields = vec![
        ("client_id", client.client_id.as_str()),
        ("client_secret", client.client_secret.as_str()),
    ];
    fields.extend_from_slice(extra);

    let response = http::post_form(TOKEN_HOST, TOKEN_PATH, &fields)
        .await
        .context("talking to Google's token endpoint")?;

    if !response.is_success() {
        bail!("{}", describe_failure(response.status, &response.body));
    }

    let token: TokenResponse = serde_json::from_str(&response.body)
        .context("Google's token response was not the JSON we expected")?;

    Ok(Grant {
        refresh_token: token.refresh_token.unwrap_or_default(),
        access_token: AccessToken {
            token: token.access_token,
            expires_at: Instant::now() + Duration::from_secs(token.expires_in),
        },
        email: token.id_token.as_deref().and_then(email_from_id_token),
    })
}

/// Turns a token-endpoint refusal into a sentence worth showing.
///
/// `invalid_grant` on its own is the single least helpful string Google
/// returns, and it is also the most common — it is what a revoked or expired
/// refresh token looks like.
fn describe_failure(status: u16, body: &str) -> String {
    let Ok(error) = serde_json::from_str::<TokenError>(body) else {
        return format!("Google returned HTTP {status}: {}", body.trim());
    };

    let hint = match error.error.as_str() {
        "invalid_grant" => Some("sign in again — the saved authorization has expired or been revoked"),
        "invalid_client" => Some("check the client ID and secret against the Google Cloud console"),
        "unauthorized_client" | "access_denied" => {
            Some("the OAuth client may still be in testing, with this address not added as a test user")
        }
        _ => None,
    };

    match (error.error_description, hint) {
        (Some(description), Some(hint)) => format!("{description} ({hint})"),
        (Some(description), None) => description,
        (None, Some(hint)) => format!("{}: {hint}", error.error),
        (None, None) => error.error,
    }
}

/// Pulls the address out of an id token without verifying its signature.
///
/// Safe here, and only here: the token arrived over a verified TLS connection
/// directly from Google in response to our own request, so there is no third
/// party who could have substituted it. It is used to label an account, never
/// to decide access.
fn email_from_id_token(id_token: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Claims {
        email: Option<String>,
    }

    let payload = id_token.split('.').nth(1)?;
    let json = base64_decode(payload)?;
    serde_json::from_slice::<Claims>(&json).ok()?.email
}

/// Waits for the browser to come back with a code. `provider` names who the
/// person is signing in with, in the messages.
///
/// Loops rather than taking the first connection: a browser will happily
/// request `/favicon.ico` against the same origin, and answering that one and
/// stopping would strand the flow.
pub(crate) async fn wait_for_redirect(
    listener: &smol::net::TcpListener,
    state: &str,
    provider: &str,
) -> Result<String> {
    let deadline = smol::Timer::after(CONSENT_TIMEOUT);
    let accept_loop = async {
        loop {
            let (mut stream, _) = listener
                .accept()
                .await
                .context("waiting for the browser to redirect back")?;

            let Some(target) = read_request_target(&mut stream).await? else {
                continue;
            };
            let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
            let params = http::parse_query(query);
            let find = |name: &str| {
                params.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
            };

            let outcome = if let Some(error) = find("error") {
                Err(match error.as_str() {
                    "access_denied" => anyhow!("sign-in was declined in the browser"),
                    other => anyhow!("{provider} refused the sign-in: {other}"),
                })
            } else if let Some(code) = find("code") {
                // The state check is what stops another page on this machine
                // from feeding us a code for an account we did not ask about.
                match find("state").as_deref() == Some(state) {
                    true => Ok(code),
                    false => Err(anyhow!("the sign-in response did not match this request")),
                }
            } else {
                // Not the redirect — a favicon, or a stray probe.
                respond(&mut stream, &format!("Waiting for {provider}\u{2026}")).await;
                continue;
            };

            // Answer before returning either way, so the tab says what
            // happened instead of showing a connection error.
            respond(
                &mut stream,
                match &outcome {
                    Ok(_) => "Signed in. You can close this tab and go back to Spool.",
                    Err(_) => "Sign-in failed. Go back to Spool for the details.",
                },
            )
            .await;

            return outcome;
        }
    };

    futures::pin_mut!(accept_loop);
    match futures::future::select(accept_loop, deadline).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => {
            bail!("timed out waiting for the browser — the sign-in was never completed")
        }
    }
}

/// Reads the request line and returns its target, e.g. `/?code=…`.
///
/// Only the first line matters, so this reads one bufferful rather than
/// draining the request: a browser sends the whole thing in one packet, and
/// waiting for headers we ignore would just be a way to hang.
async fn read_request_target(stream: &mut smol::net::TcpStream) -> Result<Option<String>> {
    let mut buffer = [0u8; 4096];
    let read = stream.read(&mut buffer).await.context("reading the redirect request")?;
    if read == 0 {
        return Ok(None);
    }

    let text = String::from_utf8_lossy(&buffer[..read]);
    Ok(text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_string))
}

/// Sends a minimal HTML page. Best effort: the flow's outcome does not depend
/// on the browser receiving it.
async fn respond(stream: &mut smol::net::TcpStream, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Spool</title>\
         <body style=\"font:16px system-ui;display:grid;place-items:center;height:90vh\">\
         <p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.ok();
    stream.flush().await.ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sasl_string_has_googles_separators() {
        assert_eq!(
            xoauth2("me@gmail.com", "ya29.abc"),
            "user=me@gmail.com\u{1}auth=Bearer ya29.abc\u{1}\u{1}"
        );
    }

    #[test]
    fn the_scope_is_the_one_that_reaches_imap() {
        // The gmail.readonly / gmail.modify scopes authenticate against the
        // REST API but are rejected by imap.gmail.com, with an error that
        // does not say so.
        assert!(SCOPE.contains("https://mail.google.com/"));
    }

    #[test]
    fn an_id_token_yields_the_address() {
        // header.payload.signature, where only the payload is read.
        let payload = crate::encoding::base64_url(br#"{"email":"me@gmail.com","sub":"1"}"#);
        let token = format!("eyJhbGciOiJSUzI1NiJ9.{payload}.signature");
        assert_eq!(email_from_id_token(&token).as_deref(), Some("me@gmail.com"));
    }

    #[test]
    fn a_malformed_id_token_is_none_rather_than_a_panic() {
        // The address is a convenience; nothing should fail because it is
        // missing or unreadable.
        assert_eq!(email_from_id_token(""), None);
        assert_eq!(email_from_id_token("a.b"), None);
        assert_eq!(email_from_id_token("a.!!!.c"), None);
        assert_eq!(email_from_id_token(&format!("a.{}.c", base64_url(b"{}"))), None);
    }

    #[test]
    fn a_token_response_parses_with_the_optional_fields_missing() {
        let json = r#"{"access_token":"ya29.a","expires_in":3599,"token_type":"Bearer"}"#;
        let token: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(token.access_token, "ya29.a");
        assert_eq!(token.refresh_token, None, "a refresh returns no new refresh token");
        assert_eq!(token.expires_in, 3599);
    }

    #[test]
    fn a_freshly_issued_token_is_fresh_and_an_old_one_is_not() {
        let fresh = AccessToken {
            token: "a".into(),
            expires_at: Instant::now() + Duration::from_secs(3600),
        };
        assert!(fresh.is_fresh());

        // Inside the margin: technically still valid, but not for long enough
        // to start a sync with.
        let expiring = AccessToken {
            token: "a".into(),
            expires_at: Instant::now() + Duration::from_secs(30),
        };
        assert!(!expiring.is_fresh(), "a token about to expire must be refreshed");
    }

    #[test]
    fn a_refusal_is_explained_rather_than_echoed() {
        let message = describe_failure(400, r#"{"error":"invalid_grant"}"#);
        assert!(message.contains("sign in again"), "{message}");

        let message = describe_failure(
            401,
            r#"{"error":"invalid_client","error_description":"The OAuth client was not found."}"#,
        );
        assert!(message.contains("The OAuth client was not found."), "{message}");
        assert!(message.contains("Google Cloud console"), "{message}");

        // Anything unparseable still surfaces the server's own words.
        let message = describe_failure(502, "<html>bad gateway</html>");
        assert!(message.contains("502"), "{message}");
        assert!(message.contains("bad gateway"), "{message}");
    }

    #[test]
    fn a_configured_client_has_the_shape_google_issues() {
        // Pasting the wrong string from the console — a project number, or a
        // *web* client instead of a desktop one — fails at the consent screen
        // with an error that does not say which of them is wrong. Cheaper to
        // notice here.
        let Ok(client) = ClientCredentials::bundled() else {
            return; // No client compiled in; that case is covered below.
        };

        assert!(
            client.client_id.ends_with(".apps.googleusercontent.com"),
            "a Google OAuth client ID ends in .apps.googleusercontent.com"
        );
        assert!(
            !client.client_secret.contains(char::is_whitespace),
            "the client secret has whitespace in it — probably a copy-paste artefact"
        );
    }

    #[test]
    fn a_build_with_no_client_says_where_to_put_one() {
        // Only meaningful when nothing is configured — which is the state of
        // a fresh checkout, and the state this message is written for.
        if ClientCredentials::bundled().is_ok() {
            return;
        }
        let error = ClientCredentials::bundled().unwrap_err().to_string();
        assert!(error.contains("oauth.rs"), "{error}");
        assert!(error.contains("SPOOL_GOOGLE_CLIENT_ID"), "{error}");
        assert!(!ClientCredentials::is_available());
    }

    #[test]
    fn a_blank_credential_counts_as_absent() {
        // An empty constant is the default state of the source file, and an
        // empty environment variable is what an unset shell variable expands
        // to in a launcher script. Neither is a client.
        assert_eq!(resolve("SPOOL_TEST_UNSET_VARIABLE", "   "), None);
        assert_eq!(resolve("SPOOL_TEST_UNSET_VARIABLE", " baked "), Some("baked".into()));
    }

    #[test]
    fn authorizing_without_a_client_id_fails_before_opening_a_browser() {
        let mut opened = false;
        let client = ClientCredentials { client_id: "  ".into(), client_secret: "s".into() };
        // Matched rather than unwrapped: `Grant` deliberately has no `Debug`,
        // so that a refresh token cannot reach a log line by accident.
        let Err(error) = smol::block_on(authorize(&client, None, |_| opened = true)) else {
            panic!("a blank client ID must not produce a grant");
        };

        assert!(error.to_string().contains("client ID"), "{error}");
        assert!(!opened, "no browser should open when the request cannot succeed");
    }
}
