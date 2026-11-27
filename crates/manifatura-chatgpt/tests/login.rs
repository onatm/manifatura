//! The whole sign-in against a fake issuer: dynamic registration, the loopback callback, the
//! code exchange, ID-token verification, refresh and the model list.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use manifatura_chatgpt::ChatGpt;
use manifatura_models::responses::Credentials;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const KEY: &[u8] = include_bytes!("data/test-key.pk8");
const MODULUS: &str = include_str!("data/test-key.modulus");

#[derive(Default)]
struct Issuer {
    url: String,
    nonce: String,
    forms: Vec<HashMap<String, String>>,
    refreshes: usize,
}

fn sign(claims: Value) -> String {
    let key = ring::signature::RsaKeyPair::from_pkcs8(KEY).unwrap();
    let head = B64.encode(json!({"alg":"RS256","kid":"k1","typ":"JWT"}).to_string());
    let body = B64.encode(claims.to_string());
    let msg = format!("{head}.{body}");
    let mut sig = vec![0; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        msg.as_bytes(),
        &mut sig,
    )
    .unwrap();
    format!("{msg}.{}", B64.encode(sig))
}

fn hex(s: &str) -> Vec<u8> {
    let s = s.trim();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

async fn start_issuer() -> Arc<Mutex<Issuer>> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let state = Arc::new(Mutex::new(Issuer {
        url: format!("http://{}", listener.local_addr().unwrap()),
        ..Default::default()
    }));
    let st = state.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let st = st.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let (head, body) = loop {
                    let n = sock.read(&mut tmp).await.unwrap();
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        while buf.len() < end + 4 + len {
                            let n = sock.read(&mut tmp).await.unwrap();
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        break (
                            head,
                            String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).to_string(),
                        );
                    }
                };
                let path = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let reply = {
                    let mut s = st.lock().unwrap();
                    match path.as_str() {
                        "/.well-known/jwks.json" => json!({"keys":[{
                            "kty":"RSA","kid":"k1","alg":"RS256",
                            "n": B64.encode(hex(MODULUS)), "e": "AQAB"
                        }]}),
                        "/api/accounts/oauth/token" => {
                            let f: HashMap<String, String> =
                                url::form_urlencoded::parse(body.as_bytes())
                                    .into_owned()
                                    .collect();
                            s.forms.push(f.clone());
                            if f["grant_type"] == "authorization_code" {
                                let id = sign(json!({
                                    "iss": s.url, "aud": f["client_id"], "sub": "user-1",
                                    "email": "a@example.com", "nonce": s.nonce,
                                    "exp": 9_999_999_999u64
                                }));
                                json!({"access_token":"at-0","refresh_token":"rt-0","id_token":id,
                                    "token_type":"Bearer","expires_in":3600,
                                    "scope":"chatgpt.tokens.use.direct email offline_access openid profile resource.invoke"})
                            } else {
                                s.refreshes += 1;
                                json!({"access_token":format!("at-{}", s.refreshes),
                                    "refresh_token":format!("rt-{}", s.refreshes),"expires_in":3600})
                            }
                        }
                        "/v1/models" => json!({"models":[
                            {"slug":"m-1","display_name":"Model One","visibility":"list"},
                            {"slug":"hidden","display_name":"H","visibility":"hide"}
                        ]}),
                        _ => Value::Null,
                    }
                };
                let body = reply.to_string();
                let out = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(out.as_bytes()).await.unwrap();
            });
        }
    });
    state
}

/// A GET the way a browser following the redirect would send it; returns the response text.
async fn browse(url: &str) -> String {
    let u = url::Url::parse(url).unwrap();
    let mut sock = tokio::net::TcpStream::connect((u.host_str().unwrap(), u.port().unwrap()))
        .await
        .unwrap();
    let target = format!("{}?{}", u.path(), u.query().unwrap_or_default());
    sock.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    sock.read_to_string(&mut out).await.unwrap();
    out
}

fn temp_path() -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!(
            "mf-chatgpt-{}-{}",
            std::process::id(),
            B64.encode(rand_bytes())
        ))
        .join("app.json")
}

fn rand_bytes() -> [u8; 6] {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 6];
    ring::rand::SystemRandom::new().fill(&mut b).unwrap();
    b
}

#[tokio::test]
async fn signs_in_refreshes_and_lists_models() {
    let issuer = start_issuer().await;
    let url = issuer.lock().unwrap().url.clone();
    let path = temp_path();
    let chatgpt = ChatGpt::with_endpoints("bottega-test", &path, &url, format!("{url}/v1"));
    assert!(chatgpt.account().is_none());

    let login = chatgpt.login().await.unwrap();
    let auth = url::Url::parse(login.url()).unwrap();
    let q: HashMap<String, String> = auth.query_pairs().into_owned().collect();
    assert_eq!(auth.path(), "/api/accounts/authorize");
    assert_eq!(q["client_id"], "dynamic_agent_client");
    assert_eq!(q["agent_name_hint"], "bottega-test");
    assert!(q["ext_agent_host_id"].starts_with("urn:uuid:"));
    assert!(q["redirect_uri"].starts_with("http://127.0.0.1:"));
    assert!(q["redirect_uri"].ends_with("/auth/callback"));
    assert!(q["scope"].contains("chatgpt.tokens.use.direct"));
    assert_eq!(q["code_challenge_method"], "S256");
    issuer.lock().unwrap().nonce = q["nonce"].clone();

    // The browser comes back with the issued client id.
    let back = format!(
        "{}?code=c-1&state={}&client_id=oaiapp_1&scope=chatgpt.tokens.use.direct+openid",
        q["redirect_uri"], q["state"]
    );
    let browser = tokio::spawn(async move { browse(&back).await });
    let account = login.finish().await.unwrap();
    assert!(browser.await.unwrap().starts_with("HTTP/1.1 200"));
    assert_eq!(account.email.as_deref(), Some("a@example.com"));
    assert_eq!(account.subject, "user-1");
    assert!(account.plan_usage);
    {
        let s = issuer.lock().unwrap();
        let f = &s.forms[0];
        assert_eq!(f["client_id"], "oaiapp_1");
        assert_eq!(f["code"], "c-1");
        assert_eq!(f["redirect_uri"], q["redirect_uri"]);
        assert!(!f["code_verifier"].is_empty());
    }

    // Saved owner-only, with the issued client id.
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["client_id"], "oaiapp_1");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    assert_eq!(chatgpt.bearer(false).await.unwrap(), "at-0");
    // A refused token is refreshed with the issued client id.
    assert_eq!(chatgpt.bearer(true).await.unwrap(), "at-1");
    {
        let s = issuer.lock().unwrap();
        let f = s.forms.last().unwrap();
        assert_eq!(f["grant_type"], "refresh_token");
        assert_eq!(f["client_id"], "oaiapp_1");
        assert_eq!(f["refresh_token"], "rt-0");
    }

    let models = chatgpt.models().await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].slug, "m-1");

    // A second sign-in reuses the registration and hints the account.
    let again = chatgpt.login().await.unwrap();
    let q2: HashMap<String, String> = url::Url::parse(again.url())
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(q2["client_id"], "oaiapp_1");
    assert!(!q2.contains_key("agent_name_hint"));
    assert_eq!(q2["login_hint"], "a@example.com");
    assert_eq!(q2["ext_agent_host_id"], q["ext_agent_host_id"]);
    drop(again);

    chatgpt.logout(false).await.unwrap();
    assert!(chatgpt.account().is_none());
    assert!(chatgpt.bearer(false).await.is_err());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
