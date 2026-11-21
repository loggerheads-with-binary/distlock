# distlock-server

[![CI](https://github.com/loggerheads-with-binary/distlock/actions/workflows/ci.yml/badge.svg)](https://github.com/loggerheads-with-binary/distlock/actions/workflows/ci.yml)
[![Release](https://github.com/loggerheads-with-binary/distlock/actions/workflows/release.yml/badge.svg)](https://github.com/loggerheads-with-binary/distlock/actions/workflows/release.yml)
[![codecov](https://codecov.io/gh/loggerheads-with-binary/distlock/graph/badge.svg?component=distlock-server)](https://codecov.io/gh/loggerheads-with-binary/distlock)
[![crates.io](https://img.shields.io/crates/v/distlock-server.svg)](https://crates.io/crates/distlock-server)
[![docs.rs](https://docs.rs/distlock-server/badge.svg)](https://docs.rs/distlock-server)

The TCP server of [distlock](https://github.com/loggerheads-with-binary/distlock), a distributed lock
service with mutexes, read/write locks, semaphores and events. It speaks a protobuf protocol and drives
one engine task that owns a [`distlock-core`](https://crates.io/crates/distlock-core) lock table.

## Install and run

```bash
cargo install distlock-server
```

1. Create a users file. It holds client ids and bcrypt hashes, never plaintext passwords.
   ```json
   {
     "version": 1,
     "users": [
       { "client_id": "billing-service", "password_hash": "$2b$12$..." }
     ]
   }
   ```
   Make hashes with [`scripts/hash-password.sh`](https://github.com/loggerheads-with-binary/distlock/blob/main/scripts/hash-password.sh).
   The format is described by [`users.schema.json`](https://github.com/loggerheads-with-binary/distlock/blob/main/config/users.schema.json).
2. Configure it through environment variables or a `.env` file in the working directory. Every setting
   and its default is listed in [`.env.example`](https://github.com/loggerheads-with-binary/distlock/blob/main/.env.example).
   The ones you will most likely set:

   | Variable | Default | Meaning |
   |---|---|---|
   | `DISTLOCK_BIND_ADDR` | `127.0.0.1:7878` | address to listen on |
   | `DISTLOCK_USERS_FILE` | `./config/users.json` | the users file |
   | `DISTLOCK_MAX_TTL_MS` | `60000` | longest lease |
   | `DISTLOCK_TLS_CERT_FILE`, `DISTLOCK_TLS_KEY_FILE` | unset | PEM certificate chain and private key: TLS on (set both or neither) |
   | `DISTLOCK_STARTUP_GRACE_MS` | `60000` | refuse lock requests this long after a start |
   | `RUST_LOG`, `DISTLOCK_LOG_FORMAT` | `info`, `text` | logging (`text` or `json`) |

3. Start it:
   ```bash
   DISTLOCK_USERS_FILE=./users.json distlock-server
   ```

## Before you deploy

* **TLS is optional.** Without a certificate the server speaks plaintext: bind to a private interface and
  never expose the port on an untrusted network. See [TLS](#tls).
* **State is in memory.** After a restart the server refuses lock requests for `DISTLOCK_STARTUP_GRACE_MS`,
  so that leases from the old process run out. Keep it at or above `DISTLOCK_MAX_TTL_MS`.
* Edit the users file and send a `reset_users` maintenance command (from the Python or Rust client) to
  reload it without a restart. Removed or disabled users are disconnected and lose their locks.

## Clients

[`distlock-client`](https://crates.io/crates/distlock-client) for Rust and
[`pydistlock`](https://pypi.org/project/pydistlock/) for Python.

Licensed under the [MIT license](https://github.com/loggerheads-with-binary/distlock/blob/main/LICENSE).

## TLS

Set `DISTLOCK_TLS_CERT_FILE` (PEM certificate chain, server certificate first) and
`DISTLOCK_TLS_KEY_FILE` (PEM private key) and the listener accepts TLS only. A plaintext client is
dropped, and a TLS client cannot talk to a server that has no certificate. Setting only one of the two
variables is a start-up error, as is a missing file or a key that does not match the certificate.
TLS 1.2 and 1.3 (rustls). The files are read once at start-up, so rotating a certificate needs a
restart. Client certificates are not requested: clients still log in with their password.

Which certificate you need depends on who the clients are:

| Situation | Server certificate | What clients need |
|---|---|---|
| Public name, certificate from a public CA (Let's Encrypt, a cloud CA) | the CA's certificate and chain | nothing: `tls=true` trusts the public roots |
| Internal service (the usual case) | signed by **your own CA** | the CA certificate (`ca_file_path` or `ca_file_content`) |
| Testing | one self-signed certificate | that certificate, as its own CA |

### Your own CA and server certificate

The commands below use OpenSSL 1.1.1+ or LibreSSL (the macOS `openssl`). Run them on a trusted machine.
`ca.key` signs every certificate your clients will accept, so keep it offline and never copy it to the
server or the clients. Only `ca.crt` is handed out.

```bash
KEY="-newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -pkeyopt ec_param_enc:named_curve -nodes"

# 1. The CA: a key and a self-signed certificate (valid 10 years).
openssl req -x509 $KEY -days 3650 -keyout ca.key -out ca.crt -subj "/CN=distlock CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"

# 2. The server's key and a signing request.
openssl req -new $KEY -keyout server.key -out server.csr -subj "/CN=distlock-server"

# 3. Sign it. List every name or IP address that clients will connect to.
cat > server.ext <<EOF
subjectAltName=DNS:locks.internal.example,IP:10.0.0.5
basicConstraints=CA:FALSE
keyUsage=digitalSignature
extendedKeyUsage=serverAuth
EOF
openssl x509 -req -sha256 -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -days 365 -extfile server.ext -out server.crt

chmod 600 server.key
DISTLOCK_TLS_CERT_FILE=server.crt DISTLOCK_TLS_KEY_FILE=server.key distlock-server
```

Then give `ca.crt` to every client (see below). To renew the server certificate, repeat steps 2 and 3
with the same CA: clients need no change. Restart the server to load it.

Things that go wrong:

* **The name.** Clients check the certificate's `subjectAltName` against the host or IP in their URL;
  the `CN` is ignored. A certificate for `locks.internal.example` is rejected by a client that connects to
  `10.0.0.5`, unless that IP is also listed.
* **`-sha256`.** The `-sha256` on step 3 matters: LibreSSL signs with SHA-1 by default, and the client
  rejects SHA-1 certificates (`UnsupportedSignatureAlgorithm`).
* **`ec_param_enc:named_curve`.** Without it LibreSSL writes the curve parameters out in full, and the
  server cannot read the key. RSA keys also work, in PKCS#8, PKCS#1 or SEC1 PEM form.
* **Intermediate CAs.** If your CA is an intermediate, put the server certificate first and the
  intermediates after it in `server.crt`. Clients are given the root.
* **Lifetime.** A certificate past `notAfter` is rejected by clients. Put the renewal in a calendar.

A single self-signed certificate (for a test) is one command; it is its own CA:

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -pkeyopt ec_param_enc:named_curve \
  -nodes -days 30 -keyout server.key -out server.crt -subj /CN=distlock \
  -addext "subjectAltName=IP:127.0.0.1,DNS:localhost"
```

### Telling clients

Each client has two independent switches, both on by default: `tls_verify_ca` (the certificate chains
to a trusted CA) and `tls_verify_host` (it names the host). Both on is full verification. Turning one
off gives "CA only" or "name only"; both off encrypts without authenticating the server. To trust your
own CA, give the client `ca.crt` as a file or as text:

```python
# Python
DistLockClient.from_url("distlock://billing:s3cret@10.0.0.5:7878?tls=true&ca_file_path=/etc/distlock/ca.crt")
DistLockClient(client_id="billing", password="s3cret", host="10.0.0.5", tls=True,
               ca_file_content=os.environ["DISTLOCK_CA_PEM"])   # CA from a secret, no file
```

```rust
// Rust
DistLockClient { tls: true, ca_file_path: Some("/etc/distlock/ca.crt".into()), ..DistLockClient::from_url(url)? }
```

```bash
# command line
distlock-health --url distlock://probe:pw@10.0.0.5:7878 --tls --ca-file /etc/distlock/ca.crt
```

Set one of `ca_file_path` and `ca_file_content`, not both. The CA file or text replaces the built-in
public roots; it may hold several certificates, for example an old CA and its replacement while you
rotate. Error messages tell you what to fix:

| Client error | Cause |
|---|---|
| `invalid peer certificate: UnknownIssuer` | the client does not trust the CA that signed the server certificate: give it `ca.crt` |
| `invalid peer certificate: NotValidForName` | the host or IP in the URL is not in the certificate's `subjectAltName` |
| `invalid peer certificate: Expired` | the certificate or the CA is past its end date, or the client's clock is wrong |
| `UnsupportedSignatureAlgorithm` | the certificate is signed with SHA-1: re-sign with `-sha256` |
| `TLS handshake failed` | the server is not speaking TLS (no certificate configured), or something else answers on that port |
| `do not form a usable pair` (server start-up) | `server.key` is not the key of `server.crt` |
