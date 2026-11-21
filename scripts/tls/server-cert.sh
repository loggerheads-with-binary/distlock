#!/usr/bin/env bash
# Make the server's certificate and private key, signed by the CA in the same folder.
#
#   scripts/tls/server-cert.sh --name locks.internal.example --ip 10.0.0.5
#   scripts/tls/server-cert.sh --name localhost --ip 127.0.0.1 --days 30
#   scripts/tls/server-cert.sh --name locks.internal.example --key /old/server.key
#
# Options: --name HOST     host name; wildcards like '*.example.com' are fine
#          --ip ADDRESS    IPv4 or IPv6 address
#          --dir DIR (default config/tls)  --days N (default 365)  --key FILE
#          --force (replace server.crt/server.key)  --help
# --name and --ip each take one value or a comma-separated list and may be repeated, so
#   --name a.example,b.example --name '*.svc.example' --ip 10.0.0.5,10.0.0.6 --ip ::1
# makes one certificate valid for all of them.
#
# List every name and address that clients put in their URL: they check it against the certificate's
# subjectAltName (the CN is ignored), so a client that connects to 10.0.0.5 needs --ip 10.0.0.5.
#
# Wildcards: '*.example.com' matches api.example.com, but not example.com itself and not
# a.b.example.com (one label only). The * must be the whole left-most label, and '*.com' is refused.
# List the bare name too if clients use it. Whoever holds the key can serve any host the wildcard
# covers, so keep its scope small.
#
# IP ranges (CIDR blocks) are not supported: a server certificate can only list single addresses.
#
# The server ALWAYS needs a private key: the certificate is public, and the key is how the server
# proves in each handshake that the certificate is its own. By default a new key is made. With
# --key FILE the certificate is made for a key you already have. The server cannot ask for a
# passphrase when it starts, so the key is written to server.key without one; if FILE is
# encrypted, its passphrase is asked for (or read from $TLS_KEY_PASSPHRASE).
#
# Needs ca.crt and ca.key in --dir (new-ca.sh or use-ca.sh). With only ca.crt it writes server.csr
# for your CA to sign; save the certificate it returns as server.crt in --dir. A CA key with a
# passphrase is read with $CA_KEY_PASSPHRASE or asked for.
set -euo pipefail
set -f  # no globbing: a name like '*' must stay text
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=_lib.sh
. "$here/_lib.sh"

dir="$DEFAULT_DIR"; days=365; key=""; sans=""; FORCE=0
add_san() {  # add one entry, once
  case ",$sans," in *",$1:$2,"*) return 0 ;; esac
  sans="${sans:+$sans,}$1:$2"
}
each_value() {  # each_value "a,b,c" FUNCTION: call FUNCTION on every item
  local item list="$1" fn="$2" IFS=,
  for item in $list; do
    item="${item# }"; item="${item% }"
    if [ -n "$item" ]; then "$fn" "$item"; fi
  done
}
add_name() {
  valid_dns_name "$1" || usage_error "not a valid host name: $1 (letters, digits, '-' and '.'; a wildcard is '*.' plus a name of at least two labels)"
  add_san DNS "$1"
}
add_ip() {
  case "$1" in */*) usage_error "$1: IP ranges (CIDR blocks) are not supported; list single addresses with --ip" ;; esac
  valid_ip "$1" || usage_error "not a valid IP address: $1"
  add_san IP "$1"
}
while [ $# -gt 0 ]; do
  case "$1" in
    --name) each_value "${2:?--name needs a value}" add_name; shift 2 ;;
    --ip) each_value "${2:?--ip needs a value}" add_ip; shift 2 ;;
    --dir) dir="${2:?--dir needs a value}"; shift 2 ;;
    --days) days="${2:?--days needs a value}"; shift 2 ;;
    --key) key="${2:?--key needs a value}"; shift 2 ;;
    --force) FORCE=1; shift ;;
    -h | --help) sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) usage_error "unknown argument $1" ;;
  esac
done
[ -n "$sans" ] || usage_error "give at least one --name or --ip"
case "$days" in '' | *[!0-9]*) usage_error "--days must be a number" ;; esac
[ -f "$dir/ca.crt" ] || die "$dir/ca.crt not found: run new-ca.sh or use-ca.sh first (same --dir)"

[ ! -f "$dir/ca.key" ] || refuse_overwrite "$dir/server.crt"  # before anything is written
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
server_key="$dir/server.key"; server_crt="$dir/server.crt"; csr="$dir/server.csr"
subj="/CN=distlock-server"  # clients ignore the CN; the names are in the subjectAltName

# --- the private key
if [ -n "$key" ]; then
  [ -f "$key" ] || die "$key: no such file"
  pass_args=""; [ -n "${TLS_KEY_PASSPHRASE:-}" ] && pass_args="-passin env:TLS_KEY_PASSPHRASE"
  # shellcheck disable=SC2086
  openssl pkey -in "$key" -out "$tmp/server.key" $pass_args 2>/dev/null \
    || die "could not read $key (wrong passphrase, or not a PEM private key)"
  if ! [ "$key" -ef "$server_key" ]; then refuse_overwrite "$server_key"; fi
  mkdir -p "$dir"; mv "$tmp/server.key" "$server_key"
  openssl req -new -key "$server_key" -subj "$subj" -out "$csr" 2>/dev/null
else
  mkdir -p "$dir"
  refuse_overwrite "$server_key"
  # shellcheck disable=SC2086
  openssl req -new $KEY_ARGS -nodes -subj "$subj" -keyout "$server_key" -out "$csr" 2>/dev/null
fi
chmod 600 "$server_key"

# --- no CA key here: hand the request to whoever has it
if [ ! -f "$dir/ca.key" ]; then
  echo "no $dir/ca.key, so this CA cannot sign here."
  echo "  1. give $csr to your CA, asking for a server certificate with subjectAltName $sans"
  echo "  2. save the certificate it returns as $server_crt (add its intermediates after it)"
  echo "private key: $server_key"
  exit 0
fi
refuse_overwrite "$server_crt"

# --- sign
cat > "$tmp/ext" <<EOF
subjectAltName=$sans
basicConstraints=CA:FALSE
keyUsage=digitalSignature
extendedKeyUsage=serverAuth
EOF
ca_pass=""; [ -n "${CA_KEY_PASSPHRASE:-}" ] && ca_pass="-passin env:CA_KEY_PASSPHRASE"
# shellcheck disable=SC2086
openssl x509 -req -sha256 -in "$csr" -CA "$dir/ca.crt" -CAkey "$dir/ca.key" $ca_pass \
  -CAserial "$dir/ca.srl" -CAcreateserial -days "$days" -extfile "$tmp/ext" -out "$server_crt" 2>/dev/null \
  || die "signing failed (does $dir/ca.key belong to $dir/ca.crt? is its passphrase in \$CA_KEY_PASSPHRASE?)"
rm -f "$csr"

# An intermediate CA's certificate goes after the server's, so clients can build the chain.
chain_note=""
if [ "$(openssl x509 -in "$dir/ca.crt" -noout -subject)" != "$(openssl x509 -in "$dir/ca.crt" -noout -issuer | sed 's/^issuer=/subject=/')" ]; then
  cat "$dir/ca.crt" >> "$server_crt"
  chain_note="  (includes the intermediate CA; clients need the root CA)"
fi
chmod 644 "$server_crt"

# --- check what we made
[ "$(pubkey_digest cert "$server_crt")" = "$(pubkey_digest key "$server_key")" ] || die "internal error: the key does not match the certificate"
if [ -z "$chain_note" ]; then openssl verify -CAfile "$dir/ca.crt" "$server_crt" >/dev/null || die "the new certificate does not verify against $dir/ca.crt"; fi

echo "server certificate for: $sans (valid $days days)"
echo "  certificate: $server_crt$chain_note"
echo "  private key: $server_key"
echo "start the server with:"
echo "  DISTLOCK_TLS_ENABLED=true DISTLOCK_TLS_CERT_FILE=$server_crt DISTLOCK_TLS_KEY_FILE=$server_key distlock-server"
echo "clients trust $dir/ca.crt (ca_file_path / --ca-file, or its text as ca_file_content)"
