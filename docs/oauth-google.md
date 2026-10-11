# Google sign-in (OAuth2)

Gmail addresses (`gmail.com`, `googlemail.com`) sign in through the browser, using the same
flow as [Microsoft](oauth-microsoft.md) ([`crates/mailcore/src/auth.rs`](../crates/mailcore/src/auth.rs)):
authorization code with PKCE, a loopback redirect, and XOAUTH2 for IMAP and SMTP. Scopes:
`https://mail.google.com/` for mail and `https://www.googleapis.com/auth/calendar` for the
calendar (CalDAV), each requested with its own access token. Builds without a Google client ID
fall back to app passwords, without calendar sync.

Accounts signed in before the calendar existed still work for mail; to add calendar access, edit
the account in Settings and save, which opens the browser sign-in again.

## One-time setup

1. In Google Cloud Console, create a project and enable the **Gmail API** and the **CalDAV API**.
2. **OAuth consent screen**: user type External, publishing status **Testing**, and add every
   test Gmail address under *Test users*. Under *Data access*, add both scopes above.
3. **Credentials → Create OAuth client → Desktop app**, then download the JSON file. It is
   git-ignored (`client_secret_*.json`), so keep it out of the repo.

`https://mail.google.com/` is a *restricted* scope. While the app is in Testing, only the listed
test users can sign in, and their refresh tokens expire after **7 days**. Opening it to everyone
requires Google's verification plus a yearly CASA security assessment.

## Build with the client

Google treats a desktop client's secret as public: it ships in every binary. Both values are
compiled in from env vars:

```sh
export MAILCLIENT_GOOGLE_CLIENT_ID=$(jq -r .installed.client_id client_secret_*.json)
export MAILCLIENT_GOOGLE_CLIENT_SECRET=$(jq -r .installed.client_secret client_secret_*.json)
cargo run
```

## Live test

```sh
LIVE_GOOGLE_EMAIL=test@gmail.com cargo test -p mailcore --test google_live -- --ignored --nocapture --test-threads=1
```

The test opens the browser sign-in, prints `LIVE_GOOGLE_REFRESH_TOKEN=…`, refreshes the access
token, lists the folders over IMAP (checking for Inbox, Sent and Trash) and sends itself a mail
over SMTP. A second test, sharing the sign-in, creates an event in the account's Google Calendar
over CalDAV, syncs it back, moves and renames it, and deletes it again. A `403 … CalDAV API has
not been used in project …` means the CalDAV API is not enabled in the Cloud project (step 1). To skip the browser on later runs, export the printed token.
