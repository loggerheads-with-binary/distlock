"""Test harness: run the real ``distlock-server`` binary and talk to it through the Python package."""

from __future__ import annotations

import os
import shutil
import socket
import subprocess
import time
import uuid
from pathlib import Path
from typing import Callable, Dict, Iterator, Optional

import pytest

import distlock

REPO = Path(__file__).resolve().parents[2]
PASSWORDS = {"billing": "pw-billing", "reports": "pw-reports"}


def _server_binary() -> Path:
    override = os.environ.get("DISTLOCK_SERVER_BIN")
    if override:
        return Path(override)
    binary = REPO / "target" / "debug" / "distlock-server"
    if not binary.exists() and not os.environ.get("DISTLOCK_NO_BUILD"):
        subprocess.run(["cargo", "build", "-p", "distlock-server"], cwd=REPO, check=True)
    if not binary.exists():
        pytest.exit(f"{binary} not found; run `cargo build -p distlock-server` (or set DISTLOCK_SERVER_BIN)", 2)
    return binary


_HASHES: Dict[str, str] = {}


def _hash(password: str) -> str:
    """bcrypt hash (cost 10, the minimum the server accepts) made by the repo's own script, cached."""
    if password not in _HASHES:
        _HASHES[password] = _make_hash(password)
    return _HASHES[password]


def users_document(users: Dict[str, bool]) -> str:
    """A users file: ``{client_id: enabled}``; every user's password is ``pw-<client_id>``."""
    import json

    entries = [
        {"client_id": user, "password_hash": _hash(f"pw-{user}"), "enabled": enabled}
        for user, enabled in users.items()
    ]
    return json.dumps({"version": 1, "users": entries})


def _make_hash(password: str) -> str:
    out = subprocess.run(
        [str(REPO / "scripts" / "hash-password.sh")],
        input=password,
        capture_output=True,
        text=True,
        env={**os.environ, "COST": "10"},
        check=True,
    )
    return out.stdout.strip()


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Server:
    """A throwaway server process. ``restart()`` brings up a fresh one (empty state) on the same port."""

    def __init__(self, workdir: Path, users_file: Path, env: Dict[str, str]) -> None:
        self.port = _free_port()
        self.users_file = users_file
        self._workdir = workdir
        self._env = {
            "PATH": os.environ.get("PATH", ""),
            "DISTLOCK_BIND_ADDR": f"127.0.0.1:{self.port}",
            "DISTLOCK_USERS_FILE": str(users_file),
            "DISTLOCK_STARTUP_GRACE_MS": "0",
            "DISTLOCK_LOCK_DELAY_MS": "0",
            "DISTLOCK_SESSION_IDLE_TIMEOUT_MS": "60000",
            "RUST_LOG": "warn",
            **env,
        }
        self._process: Optional[subprocess.Popen] = None
        self.start()

    def start(self) -> None:
        assert self._process is None
        self._process = subprocess.Popen(
            [str(_server_binary())], cwd=self._workdir, env=self._env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE
        )
        deadline = time.time() + 15
        while time.time() < deadline:
            if self._process.poll() is not None:
                raise RuntimeError(f"server exited early: {self._process.stderr.read().decode()}")  # type: ignore[union-attr]
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=0.2).close()
                return
            except OSError:
                time.sleep(0.05)
        raise RuntimeError("server did not start listening in time")

    def stop(self) -> None:
        if self._process is not None:
            self._process.terminate()
            try:
                self._process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self._process.kill()
            if self._process.stderr:
                self._process.stderr.close()
            self._process = None

    def restart(self) -> None:
        self.stop()
        self.start()

    def write_users(self, users: Dict[str, bool]) -> None:
        """Rewrite this server's users file (it does not notice until asked to reload)."""
        self.users_file.write_text(users_document(users))
        self.users_file.chmod(0o600)

    def write_raw(self, text: str) -> None:
        self.users_file.write_text(text)

    def url(self, user: str = "billing", password: Optional[str] = None, **query: object) -> str:
        pw = PASSWORDS.get(user, f"pw-{user}") if password is None else password
        qs = "&".join(f"{k}={v}" for k, v in query.items())
        return f"distlock://{user}:{pw}@127.0.0.1:{self.port}" + (f"?{qs}" if qs else "")

    def client(self, user: str = "billing", **query: object) -> "distlock.DistLockClient":
        return distlock.DistLockClient.from_url(self.url(user, **query))


@pytest.fixture(scope="session")
def users_file(tmp_path_factory: pytest.TempPathFactory) -> Path:
    if shutil.which("htpasswd") is None:
        pytest.skip("htpasswd is needed to create the test users file")
    import json

    path = tmp_path_factory.mktemp("users") / "users.json"
    users = [{"client_id": u, "password_hash": _hash(p)} for u, p in PASSWORDS.items()]
    path.write_text(json.dumps({"version": 1, "users": users}))
    path.chmod(0o600)
    return path


@pytest.fixture(scope="session")
def make_server(users_file: Path, tmp_path_factory: pytest.TempPathFactory) -> Iterator[Callable[..., Server]]:
    servers = []

    def factory(users: Optional[Dict[str, bool]] = None, **env: str) -> Server:
        """``users``: give this server its own users file, ``{client_id: enabled}`` (password
        ``pw-<client_id>``), which the test may rewrite. Otherwise it shares the default file."""
        workdir = tmp_path_factory.mktemp("server")
        file = users_file
        if users is not None:
            file = workdir / "users.json"
            file.write_text(users_document(users))
            file.chmod(0o600)
        server = Server(workdir, file, env)
        servers.append(server)
        return server

    yield factory
    for server in servers:
        server.stop()


@pytest.fixture(scope="session")
def server(make_server: Callable[..., Server]) -> Server:
    """One server shared by the tests that need no special configuration."""
    return make_server()


@pytest.fixture
def name() -> str:
    """A lock name unique to the test, so tests sharing a server cannot interfere."""
    return f"lock-{uuid.uuid4().hex[:12]}"
