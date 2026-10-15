#!/usr/bin/env bash
# Local Dovecot (CONDSTORE, QRESYNC) on port 31993 for crates/mailcore/tests/dovecot.rs, with the
# certificate of scripts/test-mailserver.sh, whose test CA the tests trust. `stop` removes it.
set -euo pipefail
dir="$(cd "$(dirname "$0")/.." && pwd)/target/test-mailserver"
name=mailclient-dovecot

if [[ "${1:-}" == stop ]]; then docker rm -f "$name" >/dev/null; exit; fi
[[ -f "$dir/server.pem" ]] || { echo "run scripts/test-mailserver.sh first (it makes the test CA)" >&2; exit 1; }
chmod 644 "$dir/server.key"

docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" -p 31993:31993 -e USER_PASSWORD=pass \
  -v "$dir/server.pem:/etc/dovecot/ssl/tls.crt:ro" -v "$dir/server.key:/etc/dovecot/ssl/tls.key:ro" \
  dovecot/dovecot:2.4.1 >/dev/null
for _ in $(seq 50); do (exec 3<>/dev/tcp/localhost/31993) 2>/dev/null && break; sleep 0.2; done
echo "Dovecot running on port 31993"
