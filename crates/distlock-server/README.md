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
Pick **one** way to get the binary. `cargo install` puts it in `~/.cargo/bin` (keep that on your `PATH`)
unless you add `--root DIR`, which puts it in `DIR/bin`.

**A. From crates.io.** Needs only the package name, no repository.
```bash
cargo install --locked distlock-server                      # ~/.cargo/bin/distlock-server
cargo install --locked distlock-server --root /opt/distlock # /opt/distlock/bin/distlock-server
```
**OR B. From a checkout** (`git clone https://github.com/loggerheads-with-binary/distlock`, then from its root).
`cargo install --git https://github.com/loggerheads-with-binary/distlock distlock-server` skips the clone.
```bash
cargo install --locked --path crates/distlock-server [--root /opt/distlock]
```
**OR C. Build it** and copy the binary anywhere.
```bash
cargo build --release -p distlock-server                    # target/release/distlock-server
install -m 755 target/release/distlock-server /opt/distlock/bin/
```

Then:
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
   | `DISTLOCK_TLS_ENABLED`, `DISTLOCK_TLS_CERT_FILE`, `DISTLOCK_TLS_KEY_FILE` | `false` | serve TLS; needs the certificate and key files, [see TLS](#tls) |
   | `DISTLOCK_STARTUP_GRACE_MS` | `60000` | refuse lock requests this long after a start |
   | `RUST_LOG`, `DISTLOCK_LOG_FORMAT` | `info`, `text` | logging (`text` or `json`) |

3. Start it:
   ```bash
   DISTLOCK_USERS_FILE=./users.json distlock-server
   ```

## Before you deploy

* **TLS is optional.** Without a certificate the server speaks plaintext: bind to a private interface and
  never expose the port on an untrusted network. See [TLS](#tls) (`DISTLOCK_TLS_ENABLED=true`).
* **State is in memory.** After a restart the server refuses lock requests for `DISTLOCK_STARTUP_GRACE_MS`,
  so that leases from the old process run out. Keep it at or above `DISTLOCK_MAX_TTL_MS`.
* Edit the users file and send a `reset_users` maintenance command (from the Python or Rust client) to
  reload it without a restart. Removed or disabled users are disconnected and lose their locks.

## Clients

[`distlock-client`](https://crates.io/crates/distlock-client) for Rust and
[`pydistlock`](https://pypi.org/project/pydistlock/) for Python.

Licensed under the [MIT license](https://github.com/loggerheads-with-binary/distlock/blob/main/LICENSE).

## TLS

TLS is off unless you turn it on explicitly:

| Variable | Meaning |
|---|---|
| `DISTLOCK_TLS_ENABLED` | `true` to serve TLS only (default `false`: plaintext) |
| `DISTLOCK_TLS_CERT_FILE` | PEM certificate chain, server certificate first |
| `DISTLOCK_TLS_KEY_FILE` | PEM private key of that certificate |

With `DISTLOCK_TLS_ENABLED=true` the server **refuses to start** (and exits with an error) if either file
is not set, cannot be read, holds no certificate, or the key does not belong to the certificate. Setting
the files without the flag is an error too, so a certificate is never silently ignored. A plaintext
client is dropped by a TLS server, and a TLS client cannot talk to a plaintext one. TLS 1.2 and 1.3
(rustls). The files are read once at start-up, so renewing a certificate needs a restart. Clients are
not asked for certificates: they still log in with their password.

The **private key is not optional**. The certificate is public; the key is how the server proves in every
handshake that the certificate is its own. A passphrase on the key is what the server cannot handle: it
has nobody to ask at start-up, so the key file must be unencrypted (the scripts below take care of that).

### Make the certificates (scripts, need only `openssl`)

The scripts are in [`scripts/tls/`](https://github.com/loggerheads-with-binary/distlock/tree/main/scripts/tls)
and write to `config/tls/` (git-ignored) unless you pass `--dir`. Run them on a trusted machine. Each has
`--help`, and none overwrites a file without `--force`.

```bash
# 1. A CA. Either make a private one ...
scripts/tls/new-ca.sh                                   # ca.crt (give to clients) and ca.key (keep secret)
# ... OR use one you already have (add --key if you want to sign here):
scripts/tls/use-ca.sh --cert /pki/ca.crt --key /pki/ca.key

# 2. The server certificate. List every name and address clients put in their URL.
scripts/tls/server-cert.sh --name locks.internal.example --ip 10.0.0.5
#    already have a key (even a passphrase-protected one)?  add: --key /old/server.key

# 3. Start the server.
DISTLOCK_TLS_ENABLED=true DISTLOCK_TLS_CERT_FILE=config/tls/server.crt \
  DISTLOCK_TLS_KEY_FILE=config/tls/server.key distlock-server
```

* **Several names and addresses, wildcards.** `--name` and `--ip` take a comma-separated list and can be
  repeated: `--name locks.example,locks-2.example --name '*.svc.example' --ip 10.0.0.5,10.0.0.6 --ip ::1`.
  A wildcard must be `*.` plus a name of at least two labels, and matches exactly one label:
  `*.svc.example` covers `a.svc.example`, but not `svc.example` and not `a.b.svc.example`. List the bare
  name too if clients use it. Whoever holds the key can answer for every host the wildcard covers.
* **No IP ranges.** A server certificate lists single addresses; there is no CIDR form (ranges exist
  only as restrictions inside a CA certificate). List each address with `--ip`.
* `ca.key` signs every certificate your clients will accept: keep it offline, never on the server or the
  clients. Only `ca.crt` is handed out. `new-ca.sh --encrypt-key` protects it with a passphrase
  (then pass `CA_KEY_PASSPHRASE` or answer the prompt when signing).
* If your CA is external and you only have its certificate, `use-ca.sh --cert ca.crt` plus `server-cert.sh`
  writes the private key and a signing request, `server.csr`. Send the request to your CA and save
  what it returns as `server.crt`.
* Public name with a certificate from a public CA (Let's Encrypt, a cloud CA)? Skip the scripts: point the
  two file variables at its certificate chain and key. Clients then need no CA file.
* Renewing: run `server-cert.sh --force` again with the same CA. Clients need no change. Restart the server.
* Intermediate CA: `server-cert.sh` appends it after the server certificate; clients are given the root.

### Tell the clients

Each client has two independent switches, both on by default: `tls_verify_ca` (the certificate chains
to a trusted CA) and `tls_verify_host` (it names the host). Both on is full verification; one off gives
"CA only" or "name only"; both off encrypts without authenticating the server. To trust your own CA give the
client `ca.crt` as a file (`ca_file_path`) or as text (`ca_file_content`), not both:

```python
DistLockClient.from_url("distlock://billing:s3cret@10.0.0.5:7878?tls=true&ca_file_path=/etc/distlock/ca.crt")
DistLockClient.from_url(url, tls=True, ca_file_content=os.environ["DISTLOCK_CA_PEM"])   # arguments beat the URL
```
```rust
DistLockClient { tls: true, ca_file_path: Some("/etc/distlock/ca.crt".into()), ..DistLockClient::from_url(url)? }
```
```bash
distlock-health --url distlock://probe:pw@10.0.0.5:7878 --tls --ca-file /etc/distlock/ca.crt
```

| Client error | Cause |
|---|---|
| `invalid peer certificate: UnknownIssuer` | the client does not trust the CA that signed the server certificate: give it `ca.crt` |
| `invalid peer certificate: NotValidForName` | the host or IP in the URL is not in the certificate: re-run `server-cert.sh` with that `--name` or `--ip` |
| `invalid peer certificate: Expired` | the certificate or the CA is past its end date, or the client's clock is wrong |
| `TLS handshake failed` | the server is not speaking TLS, or something else answers on that port |
| server exits with `TLS: ... do not form a usable pair` | `server.key` is not the key of `server.crt` |
| server exits with `DISTLOCK_TLS_ENABLED` | the flag and the two files disagree: set all three, or none |
