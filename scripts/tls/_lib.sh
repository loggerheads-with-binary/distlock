# Shared by the scripts in this folder. Source it; do not run it.
# Works with bash 3.2 (macOS) and needs OpenSSL 1.1.1+ or LibreSSL 3.1+ (for `-addext`).

die() { echo "error: $*" >&2; exit 1; }
usage_error() { echo "error: $*" >&2; echo "run with --help for usage" >&2; exit 2; }

command -v openssl >/dev/null 2>&1 || die "openssl not found"

# Key parameters every script uses. `named_curve` matters: LibreSSL otherwise writes the curve
# parameters out in full, which the server cannot read.
KEY_ARGS="-newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -pkeyopt ec_param_enc:named_curve"

# Default output folder, git-ignored, and where .env.example points.
DEFAULT_DIR="config/tls"

# refuse_overwrite FILE...: stop if any exists and --force was not given ($FORCE=1).
refuse_overwrite() {
  for f in "$@"; do
    if [ -e "$f" ] && [ "${FORCE:-0}" != 1 ]; then
      die "$f already exists (use --force to replace it)"
    fi
  done
}

# pubkey_digest KIND FILE [PASSIN]: a hash of the public key in a certificate (KIND=cert) or a
# private key (KIND=key), to tell whether the two belong together. PASSIN is an openssl -passin
# argument for an encrypted key.
pubkey_digest() {
  case "$1" in
    cert) openssl x509 -in "$2" -pubkey -noout 2>/dev/null | openssl sha256 | awk '{print $NF}' ;;
    key)
      if [ -n "${3:-}" ]; then
        openssl pkey -in "$2" -pubout -passin "$3" 2>/dev/null | openssl sha256 | awk '{print $NF}'
      else
        openssl pkey -in "$2" -pubout 2>/dev/null </dev/null | openssl sha256 | awk '{print $NF}'
      fi ;;
  esac
}

# is_ca_cert FILE: true if the certificate says CA:TRUE.
is_ca_cert() { openssl x509 -in "$1" -noout -text 2>/dev/null | grep -q 'CA:TRUE'; }

# ---- names and addresses for a certificate's subjectAltName

# valid_dns_name NAME: a host name, or a wildcard whose "*" is the whole left-most label
# ("*.example.com" covers api.example.com, but neither example.com nor a.b.example.com).
valid_dns_name() {
  case "$1" in
    '' | *[!A-Za-z0-9.*-]* | .* | *. | *..* | -* | *.-* | *-.*) return 1 ;;
    '*.'*) case "${1#*.}" in *'*'*) return 1 ;; esac
           case "${1#*.}" in *.*) ;; *) return 1 ;; esac ;;  # "*.com" would cover a whole TLD
    *'*'*) return 1 ;;
  esac
}

# valid_ipv4 ADDR: dotted quad, each part 0-255.
valid_ipv4() {
  case "$1" in *[!0-9.]* | '' | .* | *. | *..*) return 1 ;; esac
  local IFS=.
  # shellcheck disable=SC2086
  set -- $1
  [ $# -eq 4 ] || return 1
  local p
  for p in "$@"; do [ "${#p}" -le 3 ] && [ "$p" -le 255 ] || return 1; done
}

# valid_ip ADDR: IPv4, or something that looks like IPv6 (hex digits, colons, an embedded IPv4 tail).
valid_ip() {
  valid_ipv4 "$1" && return 0
  case "$1" in *:*) case "$1" in *[!0-9A-Fa-f:.]*) return 1 ;; esac; return 0 ;; esac
  return 1
}
