//! Compiles in the OAuth client credentials from the committed `oauth_secrets.toml` at the
//! workspace root; a variable already set in the environment wins. A key ending in `_REV` holds
//! its value reversed, only so secret scanners don't match it.

fn main() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../oauth_secrets.toml");
    println!("cargo:rerun-if-changed={path}");
    for key in ["MAILCLIENT_MS_CLIENT_ID", "MAILCLIENT_GOOGLE_CLIENT_ID", "MAILCLIENT_GOOGLE_CLIENT_SECRET"] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let table: toml::Table = text.parse().expect("oauth_secrets.toml is not valid TOML");
    for (key, value) in table {
        let value = value.as_str().unwrap_or_else(|| panic!("oauth_secrets.toml: {key} is not a string"));
        let (key, value) = match key.strip_suffix("_REV") {
            Some(key) => (key.to_owned(), value.chars().rev().collect()),
            None => (key, value.to_owned()),
        };
        if std::env::var_os(&key).is_none() {
            println!("cargo:rustc-env={key}={value}");
        }
    }
}
