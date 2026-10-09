#!/usr/bin/env bash
# Fills the GreenMail accounts of config/greenmail-*.toml with test mail: a few messages each
# and a five-message conversation between work@localhost and home@localhost, every sender's
# copy filed in its Sent folder as a mail client would. Run after scripts/test-mailserver.sh.
set -euo pipefail
ca="$(cd "$(dirname "$0")/.." && pwd)/target/test-mailserver/ca.pem"
tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT
run=$(date +%s) # unique Message-IDs, so seeding twice gives two conversations, not one merged one

imap() { curl -s --cacert "$ca" --user "$1@localhost:x" "imaps://localhost:3993/$2" "${@:3}"; }

# message <from-name> <from> <to> <subject> <minutes-ago> <id> <in-reply-to> <references> <body>
message() {
  printf 'From: %s <%s>\r\nTo: %s\r\nSubject: %s\r\nDate: %s\r\nMessage-ID: <%s.%s@test>\r\n' \
    "$1" "$2" "$3" "$4" "$(date -R -d "-$5 min")" "$6" "$run"
  [[ -n $7 ]] && printf 'In-Reply-To: <%s.%s@test>\r\nReferences: %s\r\n' "$7" "$run" \
    "$(for r in $8; do printf '<%s.%s@test> ' "$r" "$run"; done)"
  printf '\r\n%s\r\n' "$9"
}

# send <from-user> <from-name> <to-user> ...rest of message(): delivers, and files the sender's copy in Sent.
send() {
  local from=$1 name=$2 to=$3; shift 3
  message "$name" "$from@localhost" "$to@localhost" "$@" >"$tmp"
  curl -s --cacert "$ca" smtps://localhost:3465 --mail-from "$from@localhost" --mail-rcpt "$to@localhost" -T "$tmp"
  imap "$from" Sent -T "$tmp" >/dev/null # IMAP APPEND needs the size, so a file, not stdin
}

for user in work home club; do
  for folder in Sent Trash Archive; do imap "$user" "" -X "CREATE $folder" >/dev/null || true; done
done

# Single messages from outside.
deliver() { message "$1" "$2" "$3@localhost" "$4" "$5" "$6" "" "" "$7" |
  curl -s --cacert "$ca" smtps://localhost:3465 --mail-from "$2" --mail-rcpt "$3@localhost" -T -; }
deliver Boss boss@example.org work "Quarterly numbers" 600 q1 "Please review before Friday."
deliver Mum mum@example.org home "Sunday lunch?" 500 lunch "Are you coming on Sunday?"
deliver Alice alice@example.org work "Standup moved to 10:00" 20 standup "See you then."
deliver Coach coach@example.org club "Training cancelled" 15 training "Pitch is flooded."

# The conversation, alternating between the two accounts.
send work Work home "Weekend trip planning" 300 trip1 "" "" "Shall we go hiking on Saturday? I was thinking of the lake loop."
send home Home work "Re: Weekend trip planning" 240 trip2 trip1 "trip1" "Sounds great! What time should we leave?"
send work Work home "Re: Weekend trip planning" 180 trip3 trip2 "trip1 trip2" "Let's leave at 8. I'll bring sandwiches."
send home Home work "Re: Weekend trip planning" 120 trip4 trip3 "trip1 trip2 trip3" "Perfect. I'll check the weather forecast."
send work Work home "Re: Weekend trip planning" 60 trip5 trip4 "trip1 trip2 trip3 trip4" "Forecast says sunny. See you Saturday!"

for user in work home club; do
  echo "$user: $(imap "$user" "" -X 'STATUS INBOX (MESSAGES)' | tr -d '\r') / Sent: $(imap "$user" "" -X 'STATUS Sent (MESSAGES)' | grep -o 'MESSAGES [0-9]*')"
done
