#!/usr/bin/env bash
# Make a self-signed TLS certificate and key for the server, valid for the given IP addresses and
# host names (clients here usually connect by IP, so the IP must be in the certificate).
#
#   scripts/gen-self-signed-cert.sh 10.0.0.5 [more IPs or names...]   # writes ./config/tls/{cert,key}.pem
#   OUT=/etc/distlock DAYS=825 scripts/gen-self-signed-cert.sh 10.0.0.5 localhost
#
# Then set DISTLOCK_TLS_CERT_FILE / DISTLOCK_TLS_KEY_FILE on the server, and give clients
# tls_ca_file=<cert.pem> (or tls_verify=false to skip the check).
set -euo pipefail
[ "$#" -ge 1 ] || { echo "usage: $0 <ip-or-host> [...]" >&2; exit 2; }
out="${OUT:-./config/tls}"
days="${DAYS:-365}"
san=""
for n in "$@"; do
  case "$n" in
    *[!0-9.:a-fA-F]* | "") kind=DNS ;;                       # anything with other characters is a name
    *) kind=IP ;;
  esac
  san="${san:+$san,}$kind:$n"
done
mkdir -p "$out"
umask 077
openssl req -x509 -newkey rsa:2048 -nodes -days "$days" \
  -subj "/CN=distlock" -addext "subjectAltName=$san" \
  -keyout "$out/key.pem" -out "$out/cert.pem"
echo "wrote $out/cert.pem and $out/key.pem (subjectAltName=$san)"
