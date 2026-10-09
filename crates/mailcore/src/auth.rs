//! Login secrets: a password, or OAuth2 tokens for providers that refuse passwords
//! (Microsoft). OAuth uses the authorization code flow with PKCE and a loopback
//! redirect (RFC 8252), which works the same on desktop and Android.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::form_urlencoded;

use crate::config::{AccountConfig, Auth};

/// Application (client) ID of the Entra app registration, baked in at build time.
// shortcut: replace with the literal ID once the app is registered; it is public, not a secret.
const MICROSOFT_CLIENT_ID: Option<&str> = option_env!("MAILCLIENT_MS_CLIENT_ID");
const MICROSOFT_AUTHORIZE: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
const MICROSOFT_TOKEN_HOST: &str = "login.microsoftonline.com";
const MICROSOFT_TOKEN_PATH: &str = "/common/oauth2/v2.0/token";
const MICROSOFT_SCOPES: &str =
    "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send";

fn microsoft_client_id() -> Result<&'static str> {
    MICROSOFT_CLIENT_ID.ok_or_else(|| anyhow!("this build has no Microsoft client ID (set MAILCLIENT_MS_CLIENT_ID when building)"))
}

pub struct Tokens {
    pub access: String,
    /// Absent when the server keeps the previous refresh token valid.
    pub refresh: Option<String>,
    pub expires_in: Duration,
}

/// What the engine's connections log in with. Caches the OAuth access token and
/// keeps the stored refresh token current.
pub struct Credentials {
    account: AccountConfig,
    state: tokio::sync::Mutex<(String, Option<(String, Instant)>)>,
}

impl Credentials {
    /// Reads the password or refresh token from the keyring.
    pub fn load(account: &AccountConfig) -> Result<Credentials> {
        Ok(Credentials { account: account.clone(), state: tokio::sync::Mutex::new((account.password()?, None)) })
    }

    /// The password, or an access token valid for at least another minute.
    pub async fn secret(&self) -> Result<String> {
        let mut state = self.state.lock().await;
        if self.account.auth == Auth::Password {
            return Ok(state.0.clone());
        }
        if let Some((token, until)) = &state.1
            && Instant::now() < *until
        {
            return Ok(token.clone());
        }
        let tokens = refresh(&state.0).await.context("signing in again (remove and re-add the account if this persists)")?;
        if let Some(new) = tokens.refresh {
            // Rotated: the sliding expiry only extends for the newest refresh token.
            self.account.store_password(&new)?;
            state.0 = new;
        }
        let until = Instant::now() + tokens.expires_in.saturating_sub(Duration::from_secs(60));
        state.1 = Some((tokens.access.clone(), until));
        Ok(tokens.access)
    }
}

/// A browser sign-in in progress: open [`SignIn::url`], then await [`SignIn::finish`].
pub struct SignIn {
    pub url: String,
    listeners: Vec<TcpListener>,
    redirect_uri: String,
    verifier: String,
    state: String,
}

impl SignIn {
    pub async fn start(login_hint: &str) -> Result<SignIn> {
        let client_id = microsoft_client_id()?;
        let v4 = TcpListener::bind("127.0.0.1:0").await.context("opening sign-in listener")?;
        let port = v4.local_addr()?.port();
        let mut listeners = vec![v4];
        // Browsers may resolve "localhost" to ::1 first.
        if let Ok(v6) = TcpListener::bind(("::1", port)).await {
            listeners.push(v6);
        }
        let redirect_uri = format!("http://localhost:{port}");
        let verifier = random_token()?;
        let state = random_token()?;
        let challenge = URL_SAFE_NO_PAD.encode(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()));
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("response_mode", "query")
            .append_pair("scope", MICROSOFT_SCOPES)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("login_hint", login_hint)
            .finish();
        Ok(SignIn { url: format!("{MICROSOFT_AUTHORIZE}?{query}"), listeners, redirect_uri, verifier, state })
    }

    /// Waits for the browser to come back with a code and trades it for tokens.
    pub async fn finish(self) -> Result<Tokens> {
        let code = loop {
            let (mut stream, _) = match &self.listeners[..] {
                [v4] => v4.accept().await?,
                [v4, v6, ..] => tokio::select! { r = v4.accept() => r?, r = v6.accept() => r? },
                [] => unreachable!("start binds at least one listener"),
            };
            match read_redirect(&mut stream).await {
                Ok(Some(params)) => {
                    let result = callback_code(&params, &self.state);
                    let page = match &result {
                        Ok(_) => "Signed in. You can close this tab and return to Mail.".to_string(),
                        Err(e) => format!("Sign-in failed: {}", html_escape::encode_text(&format!("{e:#}"))),
                    };
                    respond(&mut stream, "200 OK", &page).await;
                    break result?;
                }
                // favicon.ico and other stray requests
                Ok(None) => respond(&mut stream, "404 Not Found", "").await,
                Err(e) => log::info!("sign-in listener: {e:#}"),
            }
        };
        token_request(&[
            ("client_id", microsoft_client_id()?),
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &self.redirect_uri),
            ("code_verifier", &self.verifier),
            ("scope", MICROSOFT_SCOPES),
        ])
        .await
    }
}

pub async fn refresh(refresh_token: &str) -> Result<Tokens> {
    token_request(&[
        ("client_id", microsoft_client_id()?),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("scope", MICROSOFT_SCOPES),
    ])
    .await
}

fn random_token() -> Result<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow!("no system random source"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Query parameters of a `GET /?...` request, or `None` for any other path.
async fn read_redirect(stream: &mut TcpStream) -> Result<Option<Vec<(String, String)>>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk)).await??;
        if n == 0 || buf.len() > 16 * 1024 {
            bail!("incomplete request");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let line = String::from_utf8_lossy(&buf);
    let target = line.split_whitespace().nth(1).unwrap_or_default();
    Ok(target.strip_prefix("/?").map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect()))
}

fn callback_code(params: &[(String, String)], expected_state: &str) -> Result<String> {
    let get = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    if let Some(error) = get("error") {
        bail!("{}", get("error_description").unwrap_or(error));
    }
    if get("state") != Some(expected_state) {
        bail!("sign-in response does not match this request");
    }
    get("code").map(str::to_string).ok_or_else(|| anyhow!("sign-in response has no code"))
}

async fn respond(stream: &mut TcpStream, status: &str, text: &str) {
    let body = format!("<!DOCTYPE html><meta charset=utf-8><title>Mail</title><p style=\"font:16px sans-serif\">{text}</p>");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

/// POSTs a form to the token endpoint. HTTP/1.0 keeps the response un-chunked and
/// closed after the body, so no HTTP client library is needed for this one call.
async fn token_request(form: &[(&str, &str)]) -> Result<Tokens> {
    let body = form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
    let request = format!(
        "POST {MICROSOFT_TOKEN_PATH} HTTP/1.0\r\nHost: {MICROSOFT_TOKEN_HOST}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let response = tokio::time::timeout(Duration::from_secs(30), async {
        let tcp = TcpStream::connect((MICROSOFT_TOKEN_HOST, 443)).await?;
        let name = rustls::pki_types::ServerName::try_from(MICROSOFT_TOKEN_HOST)?;
        let mut tls = crate::imap::tls_connector()?.connect(name, tcp).await?;
        tls.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        tls.read_to_end(&mut response).await?;
        anyhow::Ok(response)
    })
    .await
    .map_err(|_| anyhow!("timed out contacting {MICROSOFT_TOKEN_HOST}"))?
    .with_context(|| format!("contacting {MICROSOFT_TOKEN_HOST}"))?;
    parse_token_response(&response)
}

fn parse_token_response(response: &[u8]) -> Result<Tokens> {
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow!("malformed token response"))?;
    let parsed: TokenResponse = serde_json::from_slice(&response[split + 4..]).context("parsing token response")?;
    match parsed {
        TokenResponse { access_token: Some(access), refresh_token, expires_in, .. } => Ok(Tokens {
            access,
            refresh: refresh_token,
            expires_in: Duration::from_secs(expires_in.unwrap_or(3600)),
        }),
        TokenResponse { error, error_description, .. } => {
            Err(anyhow!("{}", error_description.or(error).unwrap_or_else(|| "token request failed".into())))
        }
    }
}

/// SASL XOAUTH2 initial response (IMAP AUTHENTICATE and SMTP AUTH).
pub fn xoauth2(user: &str, access_token: &str) -> String {
    format!("user={user}\x01auth=Bearer {access_token}\x01\x01")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_callback() {
        let params = |q: &str| form_urlencoded::parse(q.as_bytes()).into_owned().collect::<Vec<_>>();
        assert_eq!(callback_code(&params("code=abc%2F1&state=s1"), "s1").unwrap(), "abc/1");
        assert!(callback_code(&params("code=abc&state=other"), "s1").is_err());
        let denied = callback_code(&params("error=access_denied&error_description=User+cancelled"), "s1");
        assert_eq!(denied.unwrap_err().to_string(), "User cancelled");
    }

    #[test]
    fn token_responses() {
        let ok = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"access_token\":\"A\",\"refresh_token\":\"R\",\"expires_in\":3599}";
        let tokens = parse_token_response(ok).unwrap();
        assert_eq!((tokens.access.as_str(), tokens.refresh.as_deref()), ("A", Some("R")));
        let bad = b"HTTP/1.1 400 Bad Request\r\n\r\n{\"error\":\"invalid_grant\",\"error_description\":\"AADSTS70000: expired\"}";
        assert_eq!(parse_token_response(bad).err().unwrap().to_string(), "AADSTS70000: expired");
    }

    #[test]
    fn reads_browser_redirect() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let browser = tokio::spawn(async move {
                let mut s = TcpStream::connect(addr).await.unwrap();
                s.write_all(b"GET /?code=c1&state=s1 HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
                let mut page = String::new();
                s.read_to_string(&mut page).await.unwrap();
                page
            });
            let (mut stream, _) = listener.accept().await.unwrap();
            let params = read_redirect(&mut stream).await.unwrap().unwrap();
            assert_eq!(callback_code(&params, "s1").unwrap(), "c1");
            respond(&mut stream, "200 OK", "done").await;
            assert!(browser.await.unwrap().starts_with("HTTP/1.1 200 OK"));
        });
    }
}
