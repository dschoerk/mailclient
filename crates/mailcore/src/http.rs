//! Just enough HTTP for OAuth token requests, CalDAV and Microsoft Graph. HTTP/1.0 keeps
//! responses un-chunked and closed after the body, so no HTTP client library is needed.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// One request; `http://` only makes sense for servers on this machine (tests, a local CalDAV server).
pub async fn request(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response> {
    let url = url::Url::parse(url).with_context(|| format!("invalid URL {url}"))?;
    let host = url.host_str().ok_or_else(|| anyhow!("URL without host: {url}"))?.to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let mut head = format!(
        "{method} {} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: mailclient\r\nContent-Length: {}\r\n",
        &url[url::Position::BeforePath..url::Position::AfterQuery],
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut raw = head.into_bytes();
    raw.extend_from_slice(body);
    let response = tokio::time::timeout(Duration::from_secs(30), async {
        let tcp = TcpStream::connect((host.as_str(), port)).await?;
        // The request's last packet goes out at once instead of waiting for an acknowledgement.
        tcp.set_nodelay(true)?;
        match url.scheme() {
            "https" => {
                let name = rustls::pki_types::ServerName::try_from(host.clone())?;
                exchange(crate::imap::tls_connector()?.connect(name, tcp).await?, &raw).await
            }
            "http" => exchange(tcp, &raw).await,
            other => bail!("unsupported URL scheme {other}"),
        }
    })
    .await
    .map_err(|_| anyhow!("timed out contacting {host}"))?
    .with_context(|| format!("contacting {host}"))?;
    parse(&response)
}

async fn exchange(mut stream: impl AsyncRead + AsyncWrite + Unpin, request: &[u8]) -> Result<Vec<u8>> {
    stream.write_all(request).await?;
    let mut response = Vec::new();
    match stream.read_to_end(&mut response).await {
        // Google closes without TLS close_notify; a truncated body fails to parse later.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !response.is_empty() => {}
        r => {
            r?;
        }
    }
    Ok(response)
}

fn parse(response: &[u8]) -> Result<Response> {
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow!("malformed HTTP response"))?;
    let head = String::from_utf8_lossy(&response[..split]);
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line"))?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok(Response { status, headers, body: response[split + 4..].to_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_responses() {
        let r = parse(b"HTTP/1.1 207 Multi-Status\r\nContent-Type: text/xml\r\nDAV: 1, calendar-access\r\n\r\n<x/>").unwrap();
        assert_eq!((r.status, r.header("content-type"), r.text().as_str()), (207, Some("text/xml"), "<x/>"));
        assert!(parse(b"garbage").is_err());
    }

    #[test]
    fn round_trip_over_plain_http() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 4096];
                let n = s.read(&mut buf).await.unwrap();
                s.write_all(b"HTTP/1.0 200 OK\r\n\r\nhello").await.unwrap();
                String::from_utf8_lossy(&buf[..n]).into_owned()
            });
            let r = request("REPORT", &format!("http://localhost:{port}/cal/?a=1"), &[("Depth", "1")], b"body").await.unwrap();
            assert_eq!((r.status, r.text().as_str()), (200, "hello"));
            let sent = server.await.unwrap();
            assert!(sent.starts_with("REPORT /cal/?a=1 HTTP/1.0\r\n"), "{sent}");
            assert!(sent.contains("Depth: 1\r\n") && sent.ends_with("\r\n\r\nbody"), "{sent}");
        });
    }
}
