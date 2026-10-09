//! Live test of Google sign-in against a real Gmail test account. Needs a build with the
//! Google client (see docs/oauth-google.md):
//!   LIVE_GOOGLE_EMAIL=you@gmail.com cargo test -p mailcore --test google_live -- --ignored --nocapture
//! Without LIVE_GOOGLE_REFRESH_TOKEN it opens the browser sign-in and prints the refresh
//! token; export it to skip the browser on later runs (it expires after 7 days while the
//! Google app is in "Testing").

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mailcore::imap::Imap;
use mailcore::store::Role;
use mailcore::{AccountConfig, Auth, auth};

#[test]
#[ignore = "needs a Gmail test account and MAILCLIENT_GOOGLE_CLIENT_ID/_SECRET at build time"]
fn google_sign_in_imap_smtp() {
    mailcore::init_tls();
    let email = std::env::var("LIVE_GOOGLE_EMAIL").expect("set LIVE_GOOGLE_EMAIL");
    let account = AccountConfig::guess(&email);
    assert_eq!(account.auth, Auth::Google);
    assert!(
        auth::available(Auth::Google),
        "built without MAILCLIENT_GOOGLE_CLIENT_ID"
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let refresh = match std::env::var("LIVE_GOOGLE_REFRESH_TOKEN") {
            Ok(token) => token,
            Err(_) => {
                let sign_in = auth::SignIn::start(Auth::Google, &email).await.unwrap();
                eprintln!("Sign in: {}", sign_in.url);
                let _ = std::process::Command::new("xdg-open").arg(&sign_in.url).spawn();
                let tokens = tokio::time::timeout(Duration::from_secs(300), sign_in.finish()).await.expect("sign-in timed out").unwrap();
                let refresh = tokens.refresh.expect("Google returned no refresh token");
                eprintln!("LIVE_GOOGLE_REFRESH_TOKEN={refresh}");
                refresh
            }
        };
        // The path the engine takes on every connection.
        let tokens = auth::refresh(Auth::Google, &refresh).await.unwrap();
        assert!(tokens.refresh.is_none(), "Google does not rotate refresh tokens");

        let mut imap = Imap::connect(&account, &tokens.access).await.unwrap();
        let folders = imap.list_folders().await.unwrap();
        for role in [Role::Inbox, Role::Sent, Role::Trash] {
            assert!(folders.iter().any(|f| f.2 == role), "no {role:?} folder in {folders:?}");
        }
        imap.logout().await;

        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let raw = format!("From: {email}\r\nTo: {email}\r\nSubject: mailclient live test {stamp}\r\n\r\nGoogle XOAUTH2 SMTP works.\r\n");
        mailcore::smtp::send(&account, &tokens.access, raw.as_bytes(), &[email.clone()]).await.unwrap();
    });
}
