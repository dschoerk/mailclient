# Microsoft sign-in (OAuth2)

Outlook.com, Hotmail, Live, MSN and Microsoft 365 reject passwords and app passwords over
IMAP/SMTP. The client signs these accounts in through the browser instead
([`crates/mailcore/src/auth.rs`](../crates/mailcore/src/auth.rs)). The code is in place but
has **not been tested against Microsoft yet**: it needs an app registration first.

## What the code does

- The setup screen switches to "Sign in with Microsoft" for `outlook.*`, `hotmail.*`,
  `live.*` and `msn.*` addresses; for Microsoft 365 custom domains, tick the checkbox.
- The system browser opens Microsoft's sign-in page (authorization code flow with PKCE).
  Microsoft redirects to `http://localhost:<random port>`, where the app listens (RFC 8252
  loopback redirect, the same on desktop and Android).
- The refresh token is stored in the keyring entry that holds passwords for other accounts.
  Each connection gets an access token, refreshed shortly before it expires; a rotated
  refresh token replaces the stored one.
- IMAP and SMTP log in with SASL XOAUTH2. Scopes: `offline_access`,
  `https://outlook.office.com/IMAP.AccessAsUser.All`, `https://outlook.office.com/SMTP.Send`.

## One-time setup: register the app

1. **Get an Entra tenant.** A personal Microsoft account has none: signing in to the Entra
   admin center lands in the shared "Microsoft Services" tenant and fails with *"Selected user
   account does not exist in tenant 'Microsoft Services'"*. Sign up for a free Azure account
   at <https://azure.microsoft.com/free> (asks for a card for verification; the free tier
   costs nothing). This creates a "Default Directory" owned by your account.
2. Open <https://entra.microsoft.com>. If it still shows "Microsoft Services", switch to the
   new directory (gear icon / "Switch directory", top right).
3. **App registrations → New registration**
   - Name: `Mail` (shown on the consent screen)
   - Supported account types: **Accounts in any organizational directory and personal
     Microsoft accounts**. Choose this now; changing it later can break sign-in.
   - Redirect URI: platform **Public client/native (mobile & desktop)**, URI `http://localhost`.
     Microsoft accepts any port on a localhost redirect.
4. Optional, helps Microsoft 365 tenants that require admin consent: **API permissions → Add →
   Microsoft Graph → Delegated**: `IMAP.AccessAsUser.All`, `SMTP.Send`, `offline_access`.
5. Copy the **Application (client) ID** from the overview page.

No client secret is needed (public client). The client ID is not a secret.

## Build with the client ID

The ID is compiled in from `MAILCLIENT_MS_CLIENT_ID`:

```sh
MAILCLIENT_MS_CLIENT_ID=<client id> cargo run
MAILCLIENT_MS_CLIENT_ID=<client id> scripts/build-android.sh --no-default-features --target x86_64-linux-android
```

Once it works, replace `MICROSOFT_CLIENT_ID` in `auth.rs` with the literal ID so every build
has it. Without an ID, the sign-in button reports "this build has no Microsoft client ID".

## Testing checklist

- [ ] Desktop: Hotmail/Outlook.com address → browser opens → consent → "Signed in" page → mail syncs
- [ ] Sending works (SMTP XOAUTH2 on `smtp.office365.com:587`)
- [ ] Restart the app: it connects without signing in again (refresh token from the keyring)
- [ ] After more than an hour: reconnect still works (access token refresh)
- [ ] Android: browser opens, redirect reaches the app, switch back manually, mail syncs
- [ ] A Microsoft 365 account (custom domain, checkbox ticked)

## Known gaps

- If the refresh token expires (about 90 days unused) or is revoked, the only fix is to
  remove and re-add the account; there is no re-sign-in button yet.
- Android: after signing in, the user switches back to the app by hand.
- Microsoft 365 organizations may block unapproved third-party apps until an admin consents.
- Consent screens show "unverified" until publisher verification (needs a Microsoft Partner
  Network ID).
- Gmail uses the same flow: see [oauth-google.md](oauth-google.md).
