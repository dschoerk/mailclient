//! Live test of password login against a real GMX test account:
//!   LIVE_GMX_EMAIL=you@gmx.at LIVE_GMX_PASSWORD=... cargo test -p mailcore --test gmx_live -- --ignored --nocapture
//! GMX ships with IMAP/SMTP off: enable "POP3/IMAP Abruf" in the GMX web settings first.

use std::time::{SystemTime, UNIX_EPOCH};

use mailcore::imap::Imap;
use mailcore::store::Role;
use mailcore::{AccountConfig, Auth};

#[test]
#[ignore = "needs a GMX test account"]
fn gmx_password_imap_smtp() {
    mailcore::init_tls();
    let email = std::env::var("LIVE_GMX_EMAIL").expect("set LIVE_GMX_EMAIL");
    let password = std::env::var("LIVE_GMX_PASSWORD").expect("set LIVE_GMX_PASSWORD");
    let account = AccountConfig::guess(&email);
    assert_eq!(account.auth, Auth::Password);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut imap = Imap::connect(&account, &password).await.unwrap();
        let folders = imap.list_folders().await.unwrap();
        eprintln!("{folders:?}");
        for role in [Role::Inbox, Role::Sent, Role::Trash] {
            assert!(folders.iter().any(|f| f.2 == role), "no {role:?} folder in {folders:?}");
        }
        imap.logout().await;

        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let raw = format!("From: {email}\r\nTo: {email}\r\nSubject: mailclient live test {stamp}\r\n\r\nGMX SMTP works.\r\n");
        mailcore::smtp::send(&account, &password, raw.as_bytes(), &[email.clone()]).await.unwrap();
    });
}
