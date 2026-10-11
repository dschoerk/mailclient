//! Login secrets: a password, or OAuth2 tokens for providers that refuse passwords
//! (Microsoft, Google). OAuth uses the authorization code flow with PKCE and a loopback
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

/// An OAuth2 identity provider. Client IDs and Google's secret are baked in at build time from
/// env vars (or the committed oauth_secrets.toml, see build.rs); they ship in the binary anyway.
struct Provider {
    name: &'static str,
    client_id: Option<&'static str>,
    client_secret: Option<&'static str>,
    id_var: &'static str,
    authorize: &'static str,
    token_host: &'static str,
    token_path: &'static str,
    /// Scopes of the access token for IMAP and SMTP.
    scopes: &'static str,
    /// Scopes of the access token for the calendar API; asked for at sign-in too.
    calendar_scopes: &'static str,
    /// Extra authorize parameters.
    extra: &'static [(&'static str, &'static str)],
}

const MICROSOFT: Provider = Provider {
    name: "Microsoft",
    client_id: option_env!("MAILCLIENT_MS_CLIENT_ID"),
    client_secret: None,
    id_var: "MAILCLIENT_MS_CLIENT_ID",
    authorize: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
    token_host: "login.microsoftonline.com",
    token_path: "/common/oauth2/v2.0/token",
    scopes: "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send",
    // A Microsoft access token is for one resource: Graph gets its own, from the same refresh token.
    calendar_scopes: "offline_access https://graph.microsoft.com/Calendars.ReadWrite",
    extra: &[("response_mode", "query")],
};

const GOOGLE: Provider = Provider {
    name: "Google",
    client_id: option_env!("MAILCLIENT_GOOGLE_CLIENT_ID"),
    client_secret: option_env!("MAILCLIENT_GOOGLE_CLIENT_SECRET"),
    id_var: "MAILCLIENT_GOOGLE_CLIENT_ID",
    authorize: "https://accounts.google.com/o/oauth2/v2/auth",
    token_host: "oauth2.googleapis.com",
    token_path: "/token",
    scopes: "https://mail.google.com/",
    // Requested separately on refresh, so sign-ins from before the calendar keep working for mail.
    calendar_scopes: "https://www.googleapis.com/auth/calendar",
    // Without these Google returns no refresh token.
    extra: &[("access_type", "offline"), ("prompt", "consent")],
};

fn provider(auth: Auth) -> Result<&'static Provider> {
    match auth {
        Auth::Microsoft => Ok(&MICROSOFT),
        Auth::Google => Ok(&GOOGLE),
        Auth::Password => bail!("password accounts have no sign-in provider"),
    }
}

/// Whether this build can sign in with `auth`'s provider.
pub fn available(auth: Auth) -> bool {
    provider(auth).is_ok_and(|p| p.client_id.is_some())
}

impl Provider {
    fn client_id(&self) -> Result<&'static str> {
        self.client_id.ok_or_else(|| {
            anyhow!(
                "this build has no {} client ID (set {} when building)",
                self.name,
                self.id_var
            )
        })
    }

    /// Token request form: the grant-specific fields plus client credentials.
    async fn token_request(&self, grant: &[(&str, &str)]) -> Result<Tokens> {
        let mut form = vec![("client_id", self.client_id()?)];
        form.extend(self.client_secret.map(|s| ("client_secret", s)));
        form.extend_from_slice(grant);
        token_request(self.token_host, self.token_path, &form).await
    }

    /// Everything the user is asked to allow at sign-in.
    fn consent_scopes(&self) -> String {
        let mut scopes: Vec<&str> = self.scopes.split(' ').collect();
        scopes.extend(self.calendar_scopes.split(' ').filter(|s| !self.scopes.split(' ').any(|m| m == *s)));
        scopes.join(" ")
    }
}

pub struct Tokens {
    pub access: String,
    /// Absent when the server keeps the previous refresh token valid.
    pub refresh: Option<String>,
    pub expires_in: Duration,
}

/// What the engine's connections log in with. Caches the OAuth access tokens (by scopes) and
/// keeps the stored refresh token current.
pub struct Credentials {
    account: AccountConfig,
    state: tokio::sync::Mutex<(String, AccessTokens)>,
}

/// (scopes, access token, valid until).
type AccessTokens = Vec<(&'static str, String, Instant)>;

impl Credentials {
    /// Reads the password or refresh token from the keyring.
    pub fn load(account: &AccountConfig) -> Result<Credentials> {
        Ok(Credentials::with_secret(account, account.password()?))
    }

    /// With a password or refresh token at hand instead of in the keyring (tests with several accounts).
    pub fn with_secret(account: &AccountConfig, secret: String) -> Credentials {
        Credentials { account: account.clone(), state: tokio::sync::Mutex::new((secret, Vec::new())) }
    }

    /// The password, or an access token for mail valid for at least another minute.
    pub async fn secret(&self) -> Result<String> {
        match self.account.auth {
            Auth::Password => Ok(self.state.lock().await.0.clone()),
            auth => self.token(provider(auth)?.scopes).await,
        }
    }

    /// An access token for the provider's calendar API (Google CalDAV, Microsoft Graph).
    pub async fn calendar_token(&self) -> Result<String> {
        self.token(provider(self.account.auth)?.calendar_scopes)
            .await
            .context("no calendar access: open Settings, edit the account and save to sign in again")
    }

    async fn token(&self, scopes: &'static str) -> Result<String> {
        let mut state = self.state.lock().await;
        if let Some((_, token, _)) = state.1.iter().find(|(s, _, until)| *s == scopes && Instant::now() < *until) {
            return Ok(token.clone());
        }
        let tokens = refresh_for(self.account.auth, &state.0, scopes)
            .await
            .context("sign-in expired: open Settings, edit the account and save to sign in again")?;
        if let Some(new) = tokens.refresh {
            // Rotated: the sliding expiry only extends for the newest refresh token.
            self.account.store_password(&new)?;
            state.0 = new;
        }
        let until = Instant::now() + tokens.expires_in.saturating_sub(Duration::from_secs(60));
        state.1.retain(|(s, _, _)| *s != scopes);
        state.1.push((scopes, tokens.access.clone(), until));
        Ok(tokens.access)
    }
}

/// A browser sign-in in progress: open [`SignIn::url`], then await [`SignIn::finish`].
pub struct SignIn {
    pub url: String,
    provider: &'static Provider,
    listeners: Vec<TcpListener>,
    redirect_uri: String,
    verifier: String,
    state: String,
}

impl SignIn {
    pub async fn start(auth: Auth, login_hint: &str) -> Result<SignIn> {
        let provider = provider(auth)?;
        let client_id = provider.client_id()?;
        let v4 = TcpListener::bind("127.0.0.1:0")
            .await
            .context("opening sign-in listener")?;
        let port = v4.local_addr()?.port();
        let mut listeners = vec![v4];
        // Browsers may resolve "localhost" to ::1 first.
        if let Ok(v6) = TcpListener::bind(("::1", port)).await {
            listeners.push(v6);
        }
        let redirect_uri = format!("http://localhost:{port}");
        let verifier = random_token()?;
        let state = random_token()?;
        let challenge = URL_SAFE_NO_PAD.encode(ring::digest::digest(
            &ring::digest::SHA256,
            verifier.as_bytes(),
        ));
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .extend_pairs(provider.extra)
            .append_pair("scope", &provider.consent_scopes())
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("login_hint", login_hint)
            .finish();
        Ok(SignIn {
            url: format!("{}?{query}", provider.authorize),
            provider,
            listeners,
            redirect_uri,
            verifier,
            state,
        })
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
                    match &result {
                        // The app cannot bring itself to the front from the background; the
                        // browser can, through the URL scheme MainActivity registers.
                        Ok(_) if cfg!(target_os = "android") => {
                            let _ = stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: dev.mailclient.app://signed-in\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                            let _ = stream.shutdown().await;
                        }
                        Ok(_) => {
                            respond(&mut stream, "200 OK", "Signed in. You can close this tab and return to Mail.").await
                        }
                        Err(e) => {
                            let page = format!("Sign-in failed: {}", html_escape::encode_text(&format!("{e:#}")));
                            respond(&mut stream, "200 OK", &page).await
                        }
                    }
                    break result?;
                }
                // favicon.ico and other stray requests
                Ok(None) => respond(&mut stream, "404 Not Found", "").await,
                Err(e) => log::info!("sign-in listener: {e:#}"),
            }
        };
        let grant = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", &self.redirect_uri),
            ("code_verifier", &self.verifier),
            ("scope", self.provider.scopes),
        ];
        // Android cuts a background app's network while the browser is in front; retry until
        // the user switches back (the code stays valid for minutes).
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            match self.provider.token_request(&grant).await {
                Err(e) if Instant::now() < deadline && e.chain().any(|c| c.is::<std::io::Error>()) => {
                    log::info!("sign-in: {e:#}; retrying");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                result => return result,
            }
        }
    }
}

/// A new access token for mail.
pub async fn refresh(auth: Auth, refresh_token: &str) -> Result<Tokens> {
    refresh_for(auth, refresh_token, provider(auth)?.scopes).await
}

async fn refresh_for(auth: Auth, refresh_token: &str, scopes: &str) -> Result<Tokens> {
    provider(auth)?
        .token_request(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("scope", scopes)])
        .await
}

fn random_token() -> Result<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("no system random source"))?;
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
    Ok(target
        .strip_prefix("/?")
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect()))
}

fn callback_code(params: &[(String, String)], expected_state: &str) -> Result<String> {
    let get = |k: &str| {
        params
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    if let Some(error) = get("error") {
        bail!("{}", get("error_description").unwrap_or(error));
    }
    if get("state") != Some(expected_state) {
        bail!("sign-in response does not match this request");
    }
    get("code")
        .map(str::to_string)
        .ok_or_else(|| anyhow!("sign-in response has no code"))
}

async fn respond(stream: &mut TcpStream, status: &str, text: &str) {
    let body = format!(
        "<!DOCTYPE html><meta charset=utf-8><title>Mail</title><p style=\"font:16px sans-serif\">{text}</p>"
    );
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

/// POSTs a form to the token endpoint.
async fn token_request(host: &str, path: &str, form: &[(&str, &str)]) -> Result<Tokens> {
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let response = crate::http::request(
        "POST",
        &format!("https://{host}{path}"),
        &[("Content-Type", "application/x-www-form-urlencoded")],
        body.as_bytes(),
    )
    .await?;
    parse_token_response(&response.body)
}

fn parse_token_response(body: &[u8]) -> Result<Tokens> {
    let parsed: TokenResponse = serde_json::from_slice(body).context("parsing token response")?;
    match parsed {
        TokenResponse {
            access_token: Some(access),
            refresh_token,
            expires_in,
            ..
        } => Ok(Tokens {
            access,
            refresh: refresh_token,
            expires_in: Duration::from_secs(expires_in.unwrap_or(3600)),
        }),
        TokenResponse {
            error,
            error_description,
            ..
        } => Err(anyhow!(
            "{}",
            error_description
                .or(error)
                .unwrap_or_else(|| "token request failed".into())
        )),
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
        let params = |q: &str| {
            form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            callback_code(&params("code=abc%2F1&state=s1"), "s1").unwrap(),
            "abc/1"
        );
        assert!(callback_code(&params("code=abc&state=other"), "s1").is_err());
        let denied = callback_code(
            &params("error=access_denied&error_description=User+cancelled"),
            "s1",
        );
        assert_eq!(denied.unwrap_err().to_string(), "User cancelled");
    }

    #[test]
    fn token_responses() {
        let ok = b"{\"access_token\":\"A\",\"refresh_token\":\"R\",\"expires_in\":3599}";
        let tokens = parse_token_response(ok).unwrap();
        assert_eq!(
            (tokens.access.as_str(), tokens.refresh.as_deref()),
            ("A", Some("R"))
        );
        let bad = b"{\"error\":\"invalid_grant\",\"error_description\":\"AADSTS70000: expired\"}";
        assert_eq!(
            parse_token_response(bad).err().unwrap().to_string(),
            "AADSTS70000: expired"
        );
    }

    #[test]
    fn reads_browser_redirect() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let browser = tokio::spawn(async move {
                let mut s = TcpStream::connect(addr).await.unwrap();
                s.write_all(b"GET /?code=c1&state=s1 HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await
                    .unwrap();
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
