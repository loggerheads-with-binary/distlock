#!/usr/bin/env bash
# Create a private CA: a key and a self-signed CA certificate.
#
#   scripts/tls/new-ca.sh                         # config/tls/ca.key and config/tls/ca.crt
#   scripts/tls/new-ca.sh --dir /secure/ca --cn "Acme distlock CA" --days 3650
#   scripts/tls/new-ca.sh --encrypt-key           # protect ca.key with a passphrase (asked for)
#
# Options: --dir DIR (default config/tls)  --cn NAME  --days N (default 3650)
#          --encrypt-key  --force (replace an existing CA)  --help
#
# ca.crt is public: give it to every client. ca.key signs every certificate your clients will accept,
# so keep it off the server and the clients. Next: scripts/tls/server-cert.sh.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=_lib.sh
. "$here/_lib.sh"

dir="$DEFAULT_DIR"; cn="distlock CA"; days=3650; encrypt=0; FORCE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dir) dir="${2:?--dir needs a value}"; shift 2 ;;
    --cn) cn="${2:?--cn needs a value}"; shift 2 ;;
    --days) days="${2:?--days needs a value}"; shift 2 ;;
    --encrypt-key) encrypt=1; shift ;;
    --force) FORCE=1; shift ;;
    -h | --help) sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) usage_error "unknown argument $1" ;;
  esac
done
case "$days" in '' | *[!0-9]*) usage_error "--days must be a number" ;; esac

mkdir -p "$dir"
refuse_overwrite "$dir/ca.key" "$dir/ca.crt"
nodes="-nodes"; [ "$encrypt" = 1 ] && nodes=""

# shellcheck disable=SC2086  # KEY_ARGS and nodes are word lists
openssl req -x509 $KEY_ARGS $nodes -days "$days" -subj "/CN=$cn" \
  -keyout "$dir/ca.key" -out "$dir/ca.crt" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
chmod 600 "$dir/ca.key"; chmod 644 "$dir/ca.crt"

echo "CA created:"
echo "  certificate (give to clients): $dir/ca.crt"
echo "  private key (keep secret):     $dir/ca.key"
echo "next: scripts/tls/server-cert.sh --dir $dir --name HOST [--ip ADDRESS]"
