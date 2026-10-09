#!/usr/bin/env bash
# Downloads public test mail into target/testdata/ (never committed):
#  - SpamAssassin public corpus, ham folders only: real mail from 2002-2003.
#    Copyright stays with the senders; do not redistribute or send these anywhere.
#  - MIT-licensed open-source HTML email templates, for modern layouts.
set -euo pipefail
dir="$(cd "$(dirname "$0")/.." && pwd)/target/testdata"
mkdir -p "$dir/spamassassin" "$dir/templates"

for f in 20030228_easy_ham 20030228_easy_ham_2 20030228_hard_ham; do
  [[ -d "$dir/spamassassin/$f" ]] && continue
  mkdir -p "$dir/spamassassin/$f"
  curl -fsSL "https://spamassassin.apache.org/old/publiccorpus/$f.tar.bz2" \
    | tar -xj -C "$dir/spamassassin/$f" --strip-components=1
done

for repo in ActiveCampaign/postmark-templates emailmonday/Cerberus leemunroe/responsive-html-email-template; do
  name="${repo#*/}"
  [[ -d "$dir/templates/$name" ]] || git clone -q --depth 1 "https://github.com/$repo" "$dir/templates/$name"
done
echo "Test mail in $dir"
