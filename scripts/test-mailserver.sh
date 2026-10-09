#!/usr/bin/env bash
# Starts a throwaway GreenMail IMAP/SMTP server on localhost (IMAPS 3993, SMTPS 3465)
# with a certificate from a local test CA. Any login is accepted; users are created on first login.
# Usage: scripts/test-mailserver.sh [stop]
set -euo pipefail
dir="$(cd "$(dirname "$0")/.." && pwd)/target/test-mailserver"
name=mailclient-greenmail

if [[ "${1:-}" == stop ]]; then docker rm -f "$name" >/dev/null; exit; fi

mkdir -p "$dir"
cd "$dir"
if [[ ! -f keystore.p12 ]]; then
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=mailclient test CA" \
    -keyout ca.key -out ca.pem -addext basicConstraints=critical,CA:TRUE -addext keyUsage=keyCertSign 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 3650 -out server.pem \
    -extfile <(printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth") 2>/dev/null
  openssl pkcs12 -export -in server.pem -inkey server.key -certfile ca.pem -out keystore.p12 -passout pass:changeit
  chmod 644 keystore.p12
fi

docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" -p 3993:3993 -p 3465:3465 -v "$dir:/certs:ro" \
  -e GREENMAIL_OPTS="-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.auth.disabled \
     -Dgreenmail.tls.keystore.file=/certs/keystore.p12 -Dgreenmail.tls.keystore.password=changeit" \
  greenmail/standalone:2.1.14 >/dev/null
echo "GreenMail running. Trust its CA with: export MAILCLIENT_EXTRA_CA=$dir/ca.pem"
