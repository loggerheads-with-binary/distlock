#!/usr/bin/env bash
# Print a bcrypt hash for the "password_hash" field of config/users.json.
#
#   scripts/hash-password.sh                        # prompts twice, no echo
#   printf '%s' "$PW" | scripts/hash-password.sh   # reads stdin: nothing on argv or in shell history
#   COST=13 scripts/hash-password.sh                # work factor 10-31, default 12
#
# bcrypt ignores everything after the first 72 bytes of a password, so use a random password of
# at most 72 bytes, e.g.:  openssl rand -base64 33
#
# Requires `htpasswd` (preinstalled on macOS; `apache2-utils` / `httpd-tools` on Linux).
set -euo pipefail

cost="${COST:-12}"
case "$cost" in
  1[0-9] | 2[0-9] | 3[01]) ;;
  *) echo "COST must be between 10 and 31 (got '$cost')" >&2; exit 2 ;;
esac
command -v htpasswd >/dev/null || { echo "htpasswd not found" >&2; exit 1; }

if [ -t 0 ]; then
  IFS= read -r -s -p "Password: " pw; echo >&2
  IFS= read -r -s -p "Again:    " again; echo >&2
  [ "$pw" = "$again" ] || { echo "passwords do not match" >&2; exit 1; }
else
  # `|| [ -n "$pw" ]`: accept a final line without a trailing newline.
  IFS= read -r pw || [ -n "$pw" ]
fi
[ -n "$pw" ] || { echo "empty password" >&2; exit 1; }
if [ "$(printf '%s' "$pw" | wc -c)" -gt 72 ]; then
  echo "warning: bcrypt ignores everything after the first 72 bytes" >&2
fi

# htpasswd -i reads the password from stdin, -B selects bcrypt, -C sets the cost.
# It prints "user:$2y$..."; keep only the hash and normalise the $2y$ marker to $2b$.
printf '%s\n' "$pw" | htpasswd -niBC "$cost" x | cut -d: -f2- | tr -d '\n' | sed 's/^\$2y\$/$2b$/'
echo
