#!/usr/bin/env bash
# Use a CA you already have (a company CA, an earlier new-ca.sh run) instead of making one.
# It checks the files and puts them where server-cert.sh looks for them.
#
#   scripts/tls/use-ca.sh --cert /pki/ca.crt --key /pki/ca.key     # you can sign certificates here
#   scripts/tls/use-ca.sh --cert /pki/ca.crt                       # certificate only, see below
#
# Options: --cert FILE (required)  --key FILE  --dir DIR (default config/tls)  --force  --help
#
# Without --key you cannot sign here: server-cert.sh then writes a signing request (server.csr) for
# your CA to sign, and the CA's certificate is still what clients are given. An encrypted CA key is
# kept as it is; server-cert.sh asks for its passphrase (or reads $CA_KEY_PASSPHRASE).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=_lib.sh
. "$here/_lib.sh"

dir="$DEFAULT_DIR"; cert=""; key=""; FORCE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --cert) cert="${2:?--cert needs a value}"; shift 2 ;;
    --key) key="${2:?--key needs a value}"; shift 2 ;;
    --dir) dir="${2:?--dir needs a value}"; shift 2 ;;
    --force) FORCE=1; shift ;;
    -h | --help) sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) usage_error "unknown argument $1" ;;
  esac
done
[ -n "$cert" ] || usage_error "--cert is required"
[ -f "$cert" ] || die "$cert: no such file"

openssl x509 -in "$cert" -noout 2>/dev/null || die "$cert is not a PEM certificate"
is_ca_cert "$cert" || die "$cert is not a CA certificate (it lacks basicConstraints CA:TRUE)"
openssl x509 -in "$cert" -noout -checkend 0 >/dev/null || die "$cert has expired"

if [ -n "$key" ]; then
  [ -f "$key" ] || die "$key: no such file"
  pass=""; [ -n "${CA_KEY_PASSPHRASE:-}" ] && pass="env:CA_KEY_PASSPHRASE"
  [ "$(pubkey_digest cert "$cert")" = "$(pubkey_digest key "$key" "$pass")" ] \
    || die "$key is not the private key of $cert (or needs its passphrase in \$CA_KEY_PASSPHRASE)"
fi

mkdir -p "$dir"
refuse_overwrite "$dir/ca.crt"
[ -n "$key" ] && refuse_overwrite "$dir/ca.key"
cp "$cert" "$dir/ca.crt"; chmod 644 "$dir/ca.crt"
if [ -n "$key" ]; then cp "$key" "$dir/ca.key"; chmod 600 "$dir/ca.key"; fi

echo "using CA:"
echo "  certificate (give to clients): $dir/ca.crt"
if [ -n "$key" ]; then echo "  private key (keep secret):     $dir/ca.key"; else echo "  no private key: server-cert.sh will write a signing request for your CA"; fi
echo "next: scripts/tls/server-cert.sh --dir $dir --name HOST [--ip ADDRESS]"
