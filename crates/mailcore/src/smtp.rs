use anyhow::{Context, Result};
use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Certificate, Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::config::{AccountConfig, Auth, Security};

pub async fn send(
    account: &AccountConfig,
    password: &str,
    raw: &[u8],
    recipients: &[String],
) -> Result<()> {
    let mut builder = match account.smtp_security {
        Security::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&account.smtp_host)?,
        Security::StartTls => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&account.smtp_host)?
        }
    };
    if let Some(pem) = crate::imap::extra_ca_pem()? {
        let params = TlsParameters::builder(account.smtp_host.clone())
            .add_root_certificate(Certificate::from_pem(&pem)?)
            .build_rustls()?;
        builder = builder.tls(match account.smtp_security {
            Security::Tls => Tls::Wrapper(params),
            Security::StartTls => Tls::Required(params),
        });
    }
    if account.auth != Auth::Password {
        // `password` is the access token; lettre builds the XOAUTH2 string itself.
        builder = builder.authentication(vec![Mechanism::Xoauth2]);
    }
    let transport = builder
        .port(account.smtp_port)
        .credentials(Credentials::new(
            account.username.clone(),
            password.to_string(),
        ))
        .build();

    let from = account
        .email
        .parse()
        .with_context(|| format!("invalid sender address {}", account.email))?;
    let to = recipients
        .iter()
        .map(|r| r.parse().with_context(|| format!("invalid recipient {r}")))
        .collect::<Result<Vec<_>>>()?;
    let envelope = Envelope::new(Some(from), to)?;
    transport
        .send_raw(&envelope, raw)
        .await
        .context("SMTP send failed")?;
    Ok(())
}
