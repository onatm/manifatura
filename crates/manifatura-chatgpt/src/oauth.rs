//! The OAuth pieces: PKCE, the loopback callback, the token endpoint, ID-token checks.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::rand::SecureRandom;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::Error;

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn random(n: usize) -> Vec<u8> {
    let mut b = vec![0; n];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .expect("system randomness");
    b
}

pub(crate) fn random_token() -> String {
    B64.encode(random(32))
}

/// A UUIDv4 host id, as `urn:uuid:…`.
pub(crate) fn new_host_id() -> String {
    let mut b = random(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

pub(crate) struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub(crate) fn pkce() -> Pkce {
    let verifier = B64.encode(random(64));
    let challenge = B64.encode(ring::digest::digest(
        &ring::digest::SHA256,
        verifier.as_bytes(),
    ));
    Pkce {
        verifier,
        challenge,
    }
}

pub(crate) fn form(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

/// POSTs a form and returns the JSON body, or the OAuth error code and description.
pub(crate) async fn post_form(url: &str, body: String) -> Result<Value, (u16, String, String)> {
    let resp = crate::client()
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| (0, "network".to_string(), e.without_url().to_string()))?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(v);
    }
    let code = v["error"]
        .as_str()
        .or(v["error"]["code"].as_str())
        .unwrap_or("error")
        .to_string();
    let desc = v["error_description"]
        .as_str()
        .or(v["error"]["message"].as_str())
        .unwrap_or_else(|| std::str::from_utf8(&bytes).unwrap_or_default())
        .to_string();
    Err((status, code, desc))
}

// --- The loopback callback ---------------------------------------------------------------

/// What the browser brought back.
pub(crate) struct Callback {
    pub code: String,
    pub client_id: Option<String>,
    pub scope: Option<String>,
}

/// Waits for `GET /auth/callback?…` with our `state`, answering other requests with 404.
/// `done` writes the final page after the code has been used.
pub(crate) async fn wait_callback(
    listener: &TcpListener,
    state: &str,
    timeout: Duration,
) -> Result<(Callback, TcpStream), Error> {
    let wait = async {
        loop {
            let (mut sock, _) = listener.accept().await.map_err(Error::io)?;
            let target = read_target(&mut sock).await;
            let Some(q) = target.strip_prefix("/auth/callback?") else {
                let _ = respond(&mut sock, 404, "Not found.").await;
                continue;
            };
            let q: HashMap<String, String> = url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect();
            if q.get("state").map(String::as_str) != Some(state) {
                let _ = respond(&mut sock, 400, "This sign-in link has expired.").await;
                continue;
            }
            if let Some(e) = q.get("error") {
                let _ = respond(
                    &mut sock,
                    400,
                    "Sign-in was cancelled. You can close this tab.",
                )
                .await;
                return Err(if e == "access_denied" {
                    Error::new("sign-in was declined")
                } else {
                    Error::new(format!(
                        "sign-in failed: {e}{}",
                        q.get("error_description")
                            .map(|d| format!(" ({d})"))
                            .unwrap_or_default()
                    ))
                });
            }
            let Some(code) = q.get("code").cloned() else {
                let _ = respond(&mut sock, 400, "The sign-in response had no code.").await;
                continue;
            };
            return Ok((
                Callback {
                    code,
                    client_id: q.get("client_id").cloned(),
                    scope: q.get("scope").cloned(),
                },
                sock,
            ));
        }
    };
    tokio::time::timeout(timeout, wait)
        .await
        .map_err(|_| Error::new("sign-in timed out"))?
}

async fn read_target(sock: &mut TcpStream) -> String {
    let mut buf = vec![0; 16 * 1024];
    let mut n = 0;
    // The request line is all we need; stop at the end of the headers.
    while n < buf.len() {
        match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf[n..])).await {
            Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
            Ok(Ok(k)) => n += k,
        }
        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    head.split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

pub(crate) async fn respond(sock: &mut TcpStream, status: u16, text: &str) -> std::io::Result<()> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Sign in</title>\
         <body style=\"font-family:system-ui,sans-serif;display:grid;place-items:center;height:90vh\">\
         <p style=\"font-size:18px\">{text}</p>"
    );
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body.as_bytes()).await?;
    sock.shutdown().await
}

// --- ID tokens ---------------------------------------------------------------------------

fn b64_json(part: &str) -> Option<Value> {
    serde_json::from_slice(&B64.decode(part.trim_end_matches('=')).ok()?).ok()
}

/// Checks an ID token's RS256 signature against the issuer's JWKS, then its claims.
pub(crate) async fn verify_id_token(
    jwt: &str,
    jwks_url: &str,
    issuer: &str,
    client_id: &str,
    nonce: &str,
) -> Result<Value, Error> {
    let mut parts = jwt.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Error::new("the ID token is malformed"));
    };
    let header = b64_json(h).ok_or_else(|| Error::new("the ID token header is malformed"))?;
    if header["alg"] != "RS256" {
        return Err(Error::new(format!(
            "unexpected ID token algorithm {}",
            header["alg"]
        )));
    }
    let jwks: Value = crate::client()
        .get(jwks_url)
        .send()
        .await
        .map_err(|e| Error::new(format!("fetching the signing keys: {}", e.without_url())))?
        .bytes()
        .await
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| Error::new("the signing keys are unreadable"))?;
    let kid = header["kid"].as_str();
    let key = jwks["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|k| k["kty"] == "RSA" && (kid.is_none() || k["kid"].as_str() == kid))
        .ok_or_else(|| Error::new("no signing key matches the ID token"))?;
    let dec = |v: &Value| {
        v.as_str()
            .and_then(|s| B64.decode(s.trim_end_matches('=')).ok())
            .ok_or_else(|| Error::new("a signing key is malformed"))
    };
    let pk = ring::signature::RsaPublicKeyComponents {
        n: dec(&key["n"])?,
        e: dec(&key["e"])?,
    };
    let sig = B64
        .decode(s.trim_end_matches('='))
        .map_err(|_| Error::new("the ID token signature is malformed"))?;
    pk.verify(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        format!("{h}.{p}").as_bytes(),
        &sig,
    )
    .map_err(|_| Error::new("the ID token signature is invalid"))?;
    let c = b64_json(p).ok_or_else(|| Error::new("the ID token claims are malformed"))?;
    check_claims(&c, issuer, client_id, nonce, now())?;
    Ok(c)
}

pub(crate) fn check_claims(
    c: &Value,
    issuer: &str,
    client_id: &str,
    nonce: &str,
    now: u64,
) -> Result<(), Error> {
    if c["iss"].as_str() != Some(issuer) {
        return Err(Error::new("the ID token is from another issuer"));
    }
    let aud_ok = match &c["aud"] {
        Value::String(a) => a == client_id,
        Value::Array(a) => a.iter().any(|x| x.as_str() == Some(client_id)),
        _ => false,
    };
    if !aud_ok {
        return Err(Error::new("the ID token is for another client"));
    }
    if c["exp"].as_u64().unwrap_or(0) + 60 < now {
        return Err(Error::new("the ID token has expired"));
    }
    if c["nonce"].as_str() != Some(nonce) {
        return Err(Error::new("the ID token's nonce does not match"));
    }
    if c["sub"].as_str().unwrap_or_default().is_empty() {
        return Err(Error::new("the ID token has no subject"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_and_pkce() {
        let h = new_host_id();
        assert!(h.starts_with("urn:uuid:"));
        assert_eq!(h.len(), 9 + 36);
        assert_eq!(&h[9 + 14..9 + 15], "4");
        let p = pkce();
        assert_eq!(p.challenge.len(), 43);
        assert!(!p.challenge.contains('='));
    }

    #[test]
    fn claims_are_checked() {
        let c = json!({"iss":"https://i","aud":"cid","exp":1000,"nonce":"n","sub":"u"});
        assert!(check_claims(&c, "https://i", "cid", "n", 900).is_ok());
        assert!(check_claims(&c, "https://x", "cid", "n", 900).is_err());
        assert!(check_claims(&c, "https://i", "other", "n", 900).is_err());
        assert!(check_claims(&c, "https://i", "cid", "m", 900).is_err());
        assert!(check_claims(&c, "https://i", "cid", "n", 2000).is_err());
        let arr = json!({"iss":"https://i","aud":["a","cid"],"exp":1000,"nonce":"n","sub":"u"});
        assert!(check_claims(&arr, "https://i", "cid", "n", 900).is_ok());
    }
}
