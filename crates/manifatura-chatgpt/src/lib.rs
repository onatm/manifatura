//! Sign in with ChatGPT for manifatura: the user signs in through the browser and approves
//! plan usage, and the model calls of your agent run on their ChatGPT plan.
//!
//! This follows OpenAI's flow for open-source and locally run apps: the app registers itself
//! on the first sign-in (no client secret, no API key), the browser returns to a loopback
//! callback on `127.0.0.1`, and requests go to the public Responses API.
//!
//! ```no_run
//! # async fn demo() -> Result<(), manifatura_chatgpt::Error> {
//! use manifatura_chatgpt::ChatGpt;
//!
//! let chatgpt = ChatGpt::new("my-app", ChatGpt::default_path("my-app"));
//! if chatgpt.account().is_none() {
//!     let login = chatgpt.login().await?;
//!     println!("Open {}", login.url()); // or open the browser for the user
//!     let account = login.finish().await?;
//!     println!("Signed in as {:?}", account.email);
//! }
//! let models = chatgpt.models().await?;
//! let provider = chatgpt.provider(); // a manifatura Provider
//! # let _ = (models, provider);
//! # Ok(()) }
//! ```
//!
//! Tokens live in one owner-only JSON file per app ([`ChatGpt::default_path`]). Refreshes are
//! serialised across processes with a lock file beside it, since a refresh token rotates.
//!
//! Show the user "Using ChatGPT plan" near the composer and link "Manage usage" to
//! [`USAGE_URL`], as OpenAI's guidelines ask.

#![warn(missing_debug_implementations, missing_docs)]

mod oauth;
mod store;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use manifatura_agent::{ErrorKind, ProviderError};
use manifatura_models::responses::{BearerFuture, Credentials, Responses};
use serde::Serialize;
use serde_json::Value;
use tokio::net::TcpListener;

use store::{Saved, Session, Store};

/// OpenAI's OAuth issuer.
pub const ISSUER: &str = "https://auth.openai.com";
/// The API the tokens are for, and where requests go.
pub const API_BASE: &str = "https://api.openai.com/v1";
/// Where the user manages their plan usage.
pub const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
/// The scope that allows requests on the user's plan.
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
/// Refresh this long before the access token expires.
const REFRESH_MARGIN: u64 = 300;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

/// A sign-in or token failure.
#[derive(Clone, Debug)]
pub struct Error(String);

impl Error {
    fn new(m: impl Into<String>) -> Self {
        Self(m.into())
    }
    fn io(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub(crate) fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(60))
                .build()
                .expect("HTTP client")
        })
        .clone()
}

/// Who is signed in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Account {
    /// The account's email, if the ID token had one.
    pub email: Option<String>,
    /// The verified subject of the ID token.
    pub subject: String,
    /// Whether the user allowed this app to use their ChatGPT plan.
    pub plan_usage: bool,
}

/// A model the account can use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Model {
    /// The id to request, e.g. as `Agent::model`.
    pub slug: String,
    /// A name to show people.
    pub display_name: String,
}

/// Sign in with ChatGPT for one app. Cheap to clone.
#[derive(Clone, Debug)]
pub struct ChatGpt {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    app: String,
    store: Store,
    issuer: String,
    api: String,
    /// The access token handed out last, to tell a refused token from one another process
    /// already replaced.
    last: Mutex<Option<String>>,
    refreshing: tokio::sync::Mutex<()>,
}

impl ChatGpt {
    /// Sign-in for `app` (its name, shown to the user when it first registers), with its
    /// credentials at `path`.
    pub fn new(app: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::with_endpoints(app, path, ISSUER, API_BASE)
    }

    /// Like [`new`](Self::new), with another issuer and API (tests).
    pub fn with_endpoints(
        app: impl Into<String>,
        path: impl Into<PathBuf>,
        issuer: impl Into<String>,
        api: impl Into<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                app: app.into(),
                store: Store::new(path.into()),
                issuer: issuer.into().trim_end_matches('/').to_string(),
                api: api.into().trim_end_matches('/').to_string(),
                last: Mutex::new(None),
                refreshing: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// `$XDG_CONFIG_HOME/manifatura/chatgpt/<app>.json` (`~/.config/…` without it).
    pub fn default_path(app: &str) -> PathBuf {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
                    .join(".config")
            });
        config
            .join("manifatura/chatgpt")
            .join(format!("{app}.json"))
    }

    /// The credential file.
    pub fn path(&self) -> &Path {
        self.inner.store.path()
    }

    /// The signed-in account, if any.
    pub fn account(&self) -> Option<Account> {
        let s = self.inner.store.read().ok()??.session?;
        Some(account_of(&s))
    }

    /// Starts a sign-in: listens on a loopback port and builds the URL to open in the
    /// browser. Finish it with [`Login::finish`].
    pub async fn login(&self) -> Result<Login, Error> {
        let saved = self.inner.store.load().map_err(Error::io)?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(Error::io)?;
        let port = listener.local_addr().map_err(Error::io)?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
        let pkce = oauth::pkce();
        let state = oauth::random_token();
        let nonce = oauth::random_token();
        let resource = self.inner.api.clone();
        let mut params: Vec<(&str, String)> = vec![
            ("response_type", "code".into()),
            (
                "client_id",
                saved
                    .client_id
                    .clone()
                    .unwrap_or_else(|| DYNAMIC_CLIENT.into()),
            ),
            ("redirect_uri", redirect_uri.clone()),
            ("scope", SCOPES.into()),
            ("resource", resource),
            ("state", state.clone()),
            ("nonce", nonce.clone()),
            ("code_challenge", pkce.challenge.clone()),
            ("code_challenge_method", "S256".into()),
            ("ext_agent_host_id", saved.host_id.clone()),
        ];
        match (&saved.client_id, &saved.session) {
            (None, _) => params.push(("agent_name_hint", self.inner.app.clone())),
            (Some(_), Some(s)) => {
                params.push(("id_token_hint", s.id_token.clone()));
                if let Some(e) = &s.email {
                    params.push(("login_hint", e.clone()));
                }
            }
            (Some(_), None) => {}
        }
        let url = url::Url::parse_with_params(
            &format!("{}/api/accounts/authorize", self.inner.issuer),
            &params,
        )
        .map_err(|e| Error::new(e.to_string()))?;
        Ok(Login {
            chatgpt: self.clone(),
            url: url.to_string(),
            listener,
            state,
            nonce,
            verifier: pkce.verifier,
            redirect_uri,
            client_id: saved.client_id,
        })
    }

    /// Signs out: revokes the session and forgets its tokens. With `forget_registration`,
    /// also forgets this app's registration, so the next sign-in may pick another account.
    pub async fn logout(&self, forget_registration: bool) -> Result<(), Error> {
        let _g = self.inner.refreshing.lock().await;
        let _lock = self.inner.store.lock().await.map_err(Error::io)?;
        let Some(mut saved) = self.inner.store.read().map_err(Error::io)? else {
            return Ok(());
        };
        if let (Some(s), Some(cid)) = (&saved.session, &saved.client_id) {
            // Best effort: the tokens are forgotten either way.
            let _ = oauth::post_form(
                &format!("{}/api/accounts/oauth/revoke", self.inner.issuer),
                oauth::form(&[
                    ("token", &s.refresh_token),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", cid),
                ]),
            )
            .await;
        }
        saved.session = None;
        if forget_registration {
            saved.client_id = None;
        }
        self.inner.store.write(&saved).map_err(Error::io)?;
        *self.inner.last.lock().unwrap() = None;
        Ok(())
    }

    /// The models the account can use, in the order the API lists them.
    pub async fn models(&self) -> Result<Vec<Model>, Error> {
        let token = self
            .access(false)
            .await
            .map_err(|e| Error::new(e.message))?;
        let resp = client()
            .get(format!("{}/models", self.inner.api))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| Error::new(format!("listing models: {}", e.without_url())))?;
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap_or_default();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(Error::new(format!(
                "listing models failed (HTTP {status}): {v}"
            )));
        }
        let mut out = Vec::new();
        for m in v["models"].as_array().into_iter().flatten() {
            if m["visibility"] == "list"
                && let Some(slug) = m["slug"].as_str()
            {
                out.push(Model {
                    slug: slug.into(),
                    display_name: m["display_name"].as_str().unwrap_or(slug).into(),
                });
            }
        }
        if out.is_empty() {
            for m in v["data"].as_array().into_iter().flatten() {
                if let Some(id) = m["id"].as_str() {
                    out.push(Model {
                        slug: id.into(),
                        display_name: id.into(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// A provider that runs requests on the user's plan: the Responses API with this sign-in's
    /// tokens, tools grouped in a namespace, and no `max_output_tokens` (the route rejects it).
    pub fn provider(&self) -> Responses {
        Responses::new(self.inner.api.clone())
            .credentials(self.clone())
            .namespace("workspace", "Tools for the user's workspace.")
            .max_output_tokens(false)
    }

    /// A valid access token, refreshed first when it is about to expire or was refused.
    async fn access(&self, refused: bool) -> Result<String, ProviderError> {
        let auth = |m: String| ProviderError {
            kind: ErrorKind::Auth,
            message: m,
        };
        let session = |saved: Option<Saved>| -> Result<(Saved, Session), ProviderError> {
            let saved = saved.ok_or_else(|| auth("not signed in to ChatGPT".into()))?;
            let s = saved
                .session
                .clone()
                .ok_or_else(|| auth("not signed in to ChatGPT".into()))?;
            if !s.plan_usage() {
                return Err(auth(
                    "ChatGPT plan usage was not allowed for this app; sign in again and allow it"
                        .into(),
                ));
            }
            Ok((saved, s))
        };
        let read = || self.inner.store.read().map_err(|e| auth(e.to_string()));
        let last = self.inner.last.lock().unwrap().clone();
        let usable = |s: &Session| {
            let fresh = s.expires_at > oauth::now() + REFRESH_MARGIN;
            // A refused token is usable only if someone already replaced it.
            fresh && (!refused || last.as_deref() != Some(s.access_token.as_str()))
        };
        let (_, s) = session(read()?)?;
        if usable(&s) {
            *self.inner.last.lock().unwrap() = Some(s.access_token.clone());
            return Ok(s.access_token);
        }
        let _g = self.inner.refreshing.lock().await;
        let _lock = self
            .inner
            .store
            .lock()
            .await
            .map_err(|e| auth(e.to_string()))?;
        let (mut saved, s) = session(read()?)?;
        if usable(&s) {
            *self.inner.last.lock().unwrap() = Some(s.access_token.clone());
            return Ok(s.access_token);
        }
        let client_id = saved
            .client_id
            .clone()
            .ok_or_else(|| auth("the ChatGPT registration is missing; sign in again".into()))?;
        let v = oauth::post_form(
            &format!("{}/api/accounts/oauth/token", self.inner.issuer),
            oauth::form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("refresh_token", &s.refresh_token),
                ("resource", &self.inner.api),
            ]),
        )
        .await
        .map_err(|(status, code, desc)| {
            if status == 0 {
                ProviderError {
                    kind: ErrorKind::Network,
                    message: format!("refreshing the ChatGPT sign-in: {desc}"),
                }
            } else {
                auth(format!(
                    "the ChatGPT sign-in has ended ({code}); sign in again{}",
                    if desc.is_empty() {
                        String::new()
                    } else {
                        format!(": {desc}")
                    }
                ))
            }
        })?;
        let updated =
            tokens_into(&v, s).map_err(|e| auth(format!("refreshing the ChatGPT sign-in: {e}")))?;
        saved.session = Some(updated.clone());
        self.inner
            .store
            .write(&saved)
            .map_err(|e| auth(e.to_string()))?;
        *self.inner.last.lock().unwrap() = Some(updated.access_token.clone());
        Ok(updated.access_token)
    }
}

impl Credentials for ChatGpt {
    fn bearer(&self, refused: bool) -> BearerFuture<'_> {
        Box::pin(self.access(refused))
    }
}

fn account_of(s: &Session) -> Account {
    Account {
        email: s.email.clone(),
        subject: s.subject.clone(),
        plan_usage: s.plan_usage(),
    }
}

/// Applies a token response to a session (a refresh may leave out the ID token and scope).
fn tokens_into(v: &Value, mut s: Session) -> Result<Session, Error> {
    let access = v["access_token"]
        .as_str()
        .ok_or_else(|| Error::new("the response has no access token"))?;
    s.access_token = access.into();
    if let Some(r) = v["refresh_token"].as_str() {
        s.refresh_token = r.into();
    }
    if let Some(i) = v["id_token"].as_str() {
        s.id_token = i.into();
    }
    if let Some(t) = v["token_type"].as_str() {
        s.token_type = t.into();
    }
    if let Some(scope) = scopes(v) {
        s.scopes = scope;
    }
    let now = oauth::now();
    s.expires_at = now + v["expires_in"].as_u64().unwrap_or(3600);
    s.saved_at = now;
    Ok(s)
}

fn scopes(v: &Value) -> Option<Vec<String>> {
    match &v["scope"] {
        Value::String(s) => Some(s.split_whitespace().map(String::from).collect()),
        Value::Array(a) => Some(
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect(),
        ),
        _ => None,
    }
}

/// A sign-in in progress: open [`url`](Self::url) in the browser, then [`finish`](Self::finish).
#[derive(Debug)]
pub struct Login {
    chatgpt: ChatGpt,
    url: String,
    listener: TcpListener,
    state: String,
    nonce: String,
    verifier: String,
    redirect_uri: String,
    client_id: Option<String>,
}

impl Login {
    /// The URL to open in the user's browser.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits (up to 10 minutes) for the browser to come back, exchanges the code, checks the
    /// ID token and saves the session.
    pub async fn finish(self) -> Result<Account, Error> {
        let (cb, mut sock) =
            oauth::wait_callback(&self.listener, &self.state, LOGIN_TIMEOUT).await?;
        let r = self.complete(cb).await;
        let page = match &r {
            Ok(a) if a.plan_usage => "Signed in. You can close this tab and return to your editor.",
            Ok(_) => {
                "Signed in, but plan usage was not allowed. Return to your editor to try again."
            }
            Err(_) => "Sign-in failed. Return to your editor for details.",
        };
        let _ = oauth::respond(&mut sock, if r.is_ok() { 200 } else { 500 }, page).await;
        r
    }

    async fn complete(&self, cb: oauth::Callback) -> Result<Account, Error> {
        let inner = &self.chatgpt.inner;
        let client_id = match (&self.client_id, cb.client_id) {
            (None, Some(issued)) => issued,
            (None, None) => {
                return Err(Error::new(
                    "the registration did not complete (no client id came back); try again",
                ));
            }
            (Some(saved), Some(back)) if *saved != back => {
                return Err(Error::new("the sign-in came back for another client"));
            }
            (Some(saved), _) => saved.clone(),
        };
        let v = oauth::post_form(
            &format!("{}/api/accounts/oauth/token", inner.issuer),
            oauth::form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("code", &cb.code),
                ("code_verifier", &self.verifier),
                ("redirect_uri", &self.redirect_uri),
                ("resource", &inner.api),
            ]),
        )
        .await
        .map_err(|(status, code, desc)| {
            Error::new(format!("signing in failed (HTTP {status}, {code}): {desc}"))
        })?;
        let id_token = v["id_token"]
            .as_str()
            .ok_or_else(|| Error::new("the sign-in response has no ID token"))?;
        let claims = oauth::verify_id_token(
            id_token,
            &format!("{}/.well-known/jwks.json", inner.issuer),
            &inner.issuer,
            &client_id,
            &self.nonce,
        )
        .await?;
        let mut session = Session {
            issuer: inner.issuer.clone(),
            subject: claims["sub"].as_str().unwrap_or_default().into(),
            email: claims["email"].as_str().map(String::from),
            id_token: id_token.into(),
            access_token: String::new(),
            refresh_token: String::new(),
            token_type: "Bearer".into(),
            expires_at: 0,
            scopes: cb
                .scope
                .map(|s| s.split_whitespace().map(String::from).collect())
                .unwrap_or_default(),
            saved_at: 0,
        };
        session = tokens_into(&v, session)?;
        if session.refresh_token.is_empty() {
            return Err(Error::new("the sign-in response has no refresh token"));
        }
        let _lock = inner.store.lock().await.map_err(Error::io)?;
        let mut saved = inner.store.load().map_err(Error::io)?;
        saved.client_id = Some(client_id);
        saved.session = Some(session.clone());
        inner.store.write(&saved).map_err(Error::io)?;
        *inner.last.lock().unwrap() = None;
        Ok(account_of(&session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn token_responses_update_the_session() {
        let s = Session {
            issuer: "i".into(),
            subject: "u".into(),
            email: None,
            id_token: "id0".into(),
            access_token: "a0".into(),
            refresh_token: "r0".into(),
            token_type: "Bearer".into(),
            expires_at: 0,
            scopes: vec![PLAN_SCOPE.into()],
            saved_at: 0,
        };
        let s = tokens_into(
            &json!({"access_token":"a1","refresh_token":"r1","expires_in":3600}),
            s,
        )
        .unwrap();
        assert_eq!(s.access_token, "a1");
        assert_eq!(s.refresh_token, "r1");
        assert_eq!(s.id_token, "id0");
        assert!(s.plan_usage());
        assert!(s.expires_at > oauth::now() + 3000);
        let s = tokens_into(&json!({"access_token":"a2","scope":"openid email"}), s).unwrap();
        assert!(!s.plan_usage());
    }
}
