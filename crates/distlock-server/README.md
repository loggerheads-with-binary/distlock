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
   | `DISTLOCK_STARTUP_GRACE_MS` | `60000` | refuse lock requests this long after a start |
   | `RUST_LOG`, `DISTLOCK_LOG_FORMAT` | `info`, `text` | logging (`text` or `json`) |

3. Start it:
   ```bash
   DISTLOCK_USERS_FILE=./users.json distlock-server
   ```

## Before you deploy

* **TLS is optional and off by default.** Set `DISTLOCK_TLS_CERT_FILE` and `DISTLOCK_TLS_KEY_FILE` to turn it
  on (`scripts/gen-self-signed-cert.sh <ip>` makes a self-signed pair). Without it, bind to a private
  interface and never expose the port on an untrusted network. Clients that set `tls_verify=false` get
  encryption but no protection against a man in the middle; give them the certificate as `tls_ca_file` when you can.
* **State is in memory.** After a restart the server refuses lock requests for `DISTLOCK_STARTUP_GRACE_MS`,
  so that leases from the old process run out. Keep it at or above `DISTLOCK_MAX_TTL_MS`.
* Edit the users file and send a `reset_users` maintenance command (from the Python or Rust client) to
  reload it without a restart. Removed or disabled users are disconnected and lose their locks.

## Clients

[`distlock-client`](https://crates.io/crates/distlock-client) for Rust and
[`pydistlock`](https://pypi.org/project/pydistlock/) for Python.

Licensed under the [MIT license](https://github.com/loggerheads-with-binary/distlock/blob/main/LICENSE).
