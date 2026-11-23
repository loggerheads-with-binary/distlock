//! `distlock._native`: PyO3 bindings over the Rust client.
//!
//! This layer is deliberately thin and string-typed. Ergonomics (URL parsing, enums, context
//! managers, exception hierarchy) live in the pure-Python package `distlock`, which is the public
//! API. Every operation exists twice: an awaitable (`foo`) and a blocking one (`foo_sync`) that
//! releases the GIL and stays interruptible with Ctrl-C.
//!
//! Failures are raised as `NativeError(kind, message)`; `distlock.errors` translates them.

use std::sync::Arc;
use std::time::Duration;

use distlock_client::{
    Acquire, Client, DistLockClient, Error, EventSet, EventWait, Kind, Lock, Lost, Published,
    ResetUsersReport, ServerStats, SetResult, State, Wait,
};
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::{future_into_py, get_runtime};

pyo3::create_exception!(
    _native,
    NativeError,
    PyException,
    "Raised by the native layer as NativeError(kind, message)."
);

fn native_err(kind: &str, message: impl Into<String>) -> PyErr {
    NativeError::new_err((kind.to_owned(), message.into()))
}

fn err(e: Error) -> PyErr {
    native_err(e.kind(), e.to_string())
}

fn invalid(message: impl Into<String>) -> PyErr {
    native_err("invalid_argument", message)
}

/// Seconds (as Python floats) to a `Duration`, rejecting NaN, infinities and negatives.
fn secs(name: &str, value: f64) -> PyResult<Duration> {
    if !value.is_finite() || value < 0.0 {
        return Err(invalid(format!(
            "{name} must be a finite, non-negative number of seconds"
        )));
    }
    Duration::try_from_secs_f64(value).map_err(|_| invalid(format!("{name} is out of range")))
}

fn opt_secs(name: &str, value: Option<f64>) -> PyResult<Option<Duration>> {
    value.map(|v| secs(name, v)).transpose()
}

/// A wait from Python, which has already applied its own defaults: `None` is "as long as the server
/// allows" for certain, so it must not fall back to the Rust crate's process-wide defaults.
fn opt_wait(name: &str, value: Option<f64>) -> PyResult<Wait> {
    Ok(match value {
        None => Wait::Forever,
        Some(v) => Wait::For(secs(name, v)?),
    })
}

/// Run a future to completion on the shared runtime with the GIL released, waking every 100 ms to
/// let Python deliver signals. On Ctrl-C the future is dropped, which cancels the operation
/// (a queued acquire is withdrawn from the server).
fn block_on<T: Send + 'static>(
    py: Python<'_>,
    fut: impl std::future::Future<Output = T> + Send + 'static,
) -> PyResult<T> {
    let rt = get_runtime();
    let mut task = rt.spawn(fut);
    loop {
        // The timer must be created *inside* the runtime, hence the async block.
        let step = py.detach(|| {
            rt.block_on(async { tokio::time::timeout(Duration::from_millis(100), &mut task).await })
        });
        match step {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(join_error)) => return Err(native_err("internal", join_error.to_string())),
            Err(_elapsed) => {
                if let Err(interrupt) = py.check_signals() {
                    task.abort();
                    return Err(interrupt);
                }
            }
        }
    }
}

fn parse_state(value: &str) -> PyResult<State> {
    match value {
        "succeeded" => Ok(State::Succeeded),
        "failed" => Ok(State::Failed),
        "stored" => Ok(State::Stored),
        "abandoned" => Ok(State::Abandoned),
        other => Err(invalid(format!("unknown status {other:?}"))),
    }
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Succeeded => "succeeded",
        State::Failed => "failed",
        State::Stored => "stored",
        State::Abandoned => "abandoned",
    }
}

/// `ResetUsersReport` as a tuple, in field order:
/// `(added, removed, disabled, enabled, password_changed, sessions_closed, connections_closed, invoker_dropped, users_md5)`.
type ResetTuple = (u32, u32, u32, u32, u32, u32, u32, bool, String);

fn reset_tuple(r: ResetUsersReport) -> ResetTuple {
    (
        r.added,
        r.removed,
        r.disabled,
        r.enabled,
        r.password_changed,
        r.sessions_closed,
        r.connections_closed,
        r.invoker_dropped,
        r.users_md5,
    )
}

/// `(total_memory, used_memory, cpu_count, cpu_percent, load_1m, load_5m, load_15m)`.
type SystemTuple = (u64, u64, u32, f64, Option<f64>, Option<f64>, Option<f64>);
/// `(pid, cpu_percent, memory, virtual_memory, cpu_time_ms, open_files, open_files_limit)`.
type ProcessTuple = (
    u32,
    f64,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
);
/// `(counters, system, process)`; counters are `(version, uptime_ms, connections, sessions,
/// connected_clients, locks_held, requests_queued, lock_names, events, in_startup_grace,
/// startup_grace_remaining_ms, users_md5)`. Nested because PyO3 converts tuples of at most 12 items.
type StatsTuple = (
    (
        String,
        u64,
        u32,
        u32,
        u32,
        u32,
        u32,
        u32,
        u32,
        bool,
        u32,
        String,
    ),
    Option<SystemTuple>,
    Option<ProcessTuple>,
);

fn stats_tuple(s: ServerStats) -> StatsTuple {
    (
        (
            s.version,
            s.uptime_ms,
            s.connections,
            s.sessions,
            s.connected_clients,
            s.locks_held,
            s.requests_queued,
            s.lock_names,
            s.events,
            s.in_startup_grace,
            s.startup_grace_remaining_ms,
            s.users_md5,
        ),
        s.system.map(|y| {
            (
                y.total_memory_bytes,
                y.used_memory_bytes,
                y.cpu_count,
                y.cpu_percent,
                y.load_avg_1m,
                y.load_avg_5m,
                y.load_avg_15m,
            )
        }),
        s.process.map(|p| {
            (
                p.pid,
                p.cpu_percent,
                p.memory_bytes,
                p.virtual_memory_bytes,
                p.cpu_time_ms,
                p.open_files,
                p.open_files_limit,
            )
        }),
    )
}

/// A publication as a tuple: `(id, value, age_seconds)`.
type PublishedTuple = (u64, Option<String>, f64);

fn published_tuple(p: Published) -> PublishedTuple {
    (p.id, p.value, p.age.as_secs_f64())
}

/// A set's result as a tuple: `(woken, id, publish_for_seconds)`.
type SetTuple = (u32, u64, f64);

fn set_tuple(r: SetResult) -> SetTuple {
    (r.woken, r.id, r.publish_for.as_secs_f64())
}

fn build_event_wait(
    name: String,
    wait: Option<f64>,
    after: u64,
    namespace: Option<String>,
) -> PyResult<EventWait> {
    Ok(EventWait {
        name,
        wait: opt_wait("timeout", wait)?,
        after,
        namespace,
    })
}

fn build_event_set(
    name: String,
    value: Option<String>,
    publish_for: Option<f64>,
    namespace: Option<String>,
) -> PyResult<EventSet> {
    Ok(EventSet {
        name,
        value,
        publish_for: opt_secs("publish_for", publish_for)?,
        namespace,
    })
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Mutex => "mutex",
        Kind::Read => "read",
        Kind::Write => "write",
        Kind::Semaphore { .. } => "semaphore",
    }
}

#[allow(clippy::too_many_arguments)]
fn build_acquire(
    name: String,
    mode: &str,
    ttl: Option<f64>,
    wait: Option<f64>,
    namespace: Option<String>,
    capacity: u32,
    permits: u32,
) -> PyResult<Acquire> {
    let kind = match mode {
        "mutex" => Kind::Mutex,
        "read" => Kind::Read,
        "write" => Kind::Write,
        "semaphore" => Kind::Semaphore { capacity, permits },
        other => return Err(invalid(format!("unknown lock mode {other:?}"))),
    };
    Ok(Acquire {
        name,
        kind,
        ttl: opt_secs("ttl", ttl)?,
        wait: opt_wait("wait", wait)?,
        namespace,
    })
}

// ------------------------------------------------------------------------------------ client

/// The settings plus the (lazily created) connected client.
struct ClientCell {
    config: DistLockClient,
    client: tokio::sync::OnceCell<Client>,
}

impl ClientCell {
    /// Connect on first use. A failed attempt leaves the cell empty, so the next call retries.
    async fn get(&self) -> Result<&Client, Error> {
        self.client
            .get_or_try_init(|| self.config.clone().connect())
            .await
    }
}

#[pyclass(frozen, module = "distlock._native")]
struct NativeClient {
    cell: Arc<ClientCell>,
}

#[pymethods]
impl NativeClient {
    #[new]
    #[pyo3(signature = (client_id, password, host, port, namespace, pool_size, tls, tls_verify_ca, tls_verify_host, ca_file_path, ca_file_content, auth_mode, min_version, default_ttl))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        client_id: String,
        password: String,
        host: String,
        port: u16,
        namespace: Option<String>,
        pool_size: usize,
        tls: bool,
        tls_verify_ca: bool,
        tls_verify_host: bool,
        ca_file_path: Option<String>,
        ca_file_content: Option<String>,
        auth_mode: u32,
        min_version: Option<String>,
        default_ttl: f64,
    ) -> PyResult<Self> {
        let config = DistLockClient {
            client_id,
            password,
            host,
            port,
            namespace,
            pool_size,
            tls,
            tls_verify_ca,
            tls_verify_host,
            ca_file_path: ca_file_path.map(Into::into),
            ca_file_content,
            auth_mode,
            min_version,
            default_ttl: secs("default_ttl", default_ttl)?,
        };
        Ok(Self {
            cell: Arc::new(ClientCell {
                config,
                client: tokio::sync::OnceCell::new(),
            }),
        })
    }

    /// The server-assigned session id, or `None` until the first connection.
    #[getter]
    fn session_id(&self) -> Option<u64> {
        self.cell.client.get().and_then(Client::session_id)
    }

    /// Ask the server to reload its users file; throws out users that lost access. See
    /// `distlock.Maintenance.reset_users`.
    fn reset_users<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cell = Arc::clone(&self.cell);
        future_into_py(py, async move {
            let client = cell.get().await.map_err(err)?;
            client
                .maintenance()
                .reset_users()
                .await
                .map(reset_tuple)
                .map_err(err)
        })
    }

    fn reset_users_sync(&self, py: Python<'_>) -> PyResult<ResetTuple> {
        let cell = Arc::clone(&self.cell);
        let report = block_on(py, async move {
            cell.get().await?.maintenance().reset_users().await
        })?;
        report.map(reset_tuple).map_err(err)
    }

    /// The server's health figures. See `distlock.DistLockClient.stats`.
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cell = Arc::clone(&self.cell);
        future_into_py(py, async move {
            let client = cell.get().await.map_err(err)?;
            client.stats().await.map(stats_tuple).map_err(err)
        })
    }

    fn stats_sync(&self, py: Python<'_>) -> PyResult<StatsTuple> {
        let cell = Arc::clone(&self.cell);
        let stats = block_on(py, async move { cell.get().await?.stats().await })?;
        stats.map(stats_tuple).map_err(err)
    }

    /// Pool connections that are up right now.
    #[getter]
    fn connections(&self) -> usize {
        self.cell.client.get().map_or(0, Client::connections)
    }

    /// How many times a pool connection was re-established after dropping.
    #[getter]
    fn reconnects(&self) -> u64 {
        self.cell.client.get().map_or(0, Client::reconnects)
    }

    /// Round-trip time to the server in seconds.
    fn ping<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cell = Arc::clone(&self.cell);
        future_into_py(py, async move {
            let client = cell.get().await.map_err(err)?;
            client.ping().await.map(|d| d.as_secs_f64()).map_err(err)
        })
    }

    fn ping_sync(&self, py: Python<'_>) -> PyResult<f64> {
        let cell = Arc::clone(&self.cell);
        let rtt = block_on(py, async move { cell.get().await?.ping().await })?;
        rtt.map(|d| d.as_secs_f64()).map_err(err)
    }

    fn connect<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cell = Arc::clone(&self.cell);
        future_into_py(py, async move { cell.get().await.map(|_| ()).map_err(err) })
    }

    fn connect_sync(&self, py: Python<'_>) -> PyResult<()> {
        let cell = Arc::clone(&self.cell);
        block_on(py, async move { cell.get().await.map(|_| ()) })?.map_err(err)
    }

    #[pyo3(signature = (name, mode, ttl, wait, namespace, capacity=0, permits=1))]
    #[allow(clippy::too_many_arguments)]
    fn acquire<'py>(
        &self,
        py: Python<'py>,
        name: String,
        mode: &str,
        ttl: Option<f64>,
        wait: Option<f64>,
        namespace: Option<String>,
        capacity: u32,
        permits: u32,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request = build_acquire(name, mode, ttl, wait, namespace, capacity, permits)?;
        let cell = Arc::clone(&self.cell);
        future_into_py(py, async move {
            let client = cell.get().await.map_err(err)?;
            client
                .acquire(request)
                .await
                .map(NativeLock::new)
                .map_err(err)
        })
    }

    #[pyo3(signature = (name, mode, ttl, wait, namespace, capacity=0, permits=1))]
    #[allow(clippy::too_many_arguments)]
    fn acquire_sync(
        &self,
        py: Python<'_>,
        name: String,
        mode: &str,
        ttl: Option<f64>,
        wait: Option<f64>,
        namespace: Option<String>,
        capacity: u32,
        permits: u32,
    ) -> PyResult<NativeLock> {
        let request = build_acquire(name, mode, ttl, wait, namespace, capacity, permits)?;
        let cell = Arc::clone(&self.cell);
        let lock = block_on(py, async move { cell.get().await?.acquire(request).await })?;
        lock.map(NativeLock::new).map_err(err)
    }

    #[pyo3(signature = (name, mode, ttl, namespace, capacity=0, permits=1))]
    #[allow(clippy::too_many_arguments)]
    fn try_acquire<'py>(
        &self,
        py: Python<'py>,
        name: String,
        mode: &str,
        ttl: Option<f64>,
        namespace: Option<String>,
        capacity: u32,
        permits: u32,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request = build_acquire(name, mode, ttl, None, namespace, capacity, permits)?;
        let cell = Arc::clone(&self.cell);
        future_into_py(py, async move {
            let client = cell.get().await.map_err(err)?;
            client
                .try_acquire(request)
                .await
                .map(|l| l.map(NativeLock::new))
                .map_err(err)
        })
    }

    #[pyo3(signature = (name, mode, ttl, namespace, capacity=0, permits=1))]
    #[allow(clippy::too_many_arguments)]
    fn try_acquire_sync(
        &self,
        py: Python<'_>,
        name: String,
        mode: &str,
        ttl: Option<f64>,
        namespace: Option<String>,
        capacity: u32,
        permits: u32,
    ) -> PyResult<Option<NativeLock>> {
        let request = build_acquire(name, mode, ttl, None, namespace, capacity, permits)?;
        let cell = Arc::clone(&self.cell);
        let lock = block_on(
            py,
            async move { cell.get().await?.try_acquire(request).await },
        )?;
        lock.map(|l| l.map(NativeLock::new)).map_err(err)
    }

    /// Wait for an event: `(publication_id, value, age_seconds)`. See `distlock.Event`.
    #[pyo3(signature = (name, timeout, after, namespace))]
    fn wait_event<'py>(
        &self,
        py: Python<'py>,
        name: String,
        timeout: Option<f64>,
        after: u64,
        namespace: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request = build_event_wait(name, timeout, after, namespace)?;
        let cell = Arc::clone(&self.cell);
        future_into_py(py, async move {
            let client = cell.get().await.map_err(err)?;
            client
                .wait_event(request)
                .await
                .map(published_tuple)
                .map_err(err)
        })
    }

    #[pyo3(signature = (name, timeout, after, namespace))]
    fn wait_event_sync(
        &self,
        py: Python<'_>,
        name: String,
        timeout: Option<f64>,
        after: u64,
        namespace: Option<String>,
    ) -> PyResult<PublishedTuple> {
        let request = build_event_wait(name, timeout, after, namespace)?;
        let cell = Arc::clone(&self.cell);
        let published = block_on(
            py,
            async move { cell.get().await?.wait_event(request).await },
        )?;
        published.map(published_tuple).map_err(err)
    }

    /// Set an event: `(woken, publication_id, publish_for_seconds)`. See `distlock.Event`.
    #[pyo3(signature = (name, value, publish_for, namespace))]
    fn set_event<'py>(
        &self,
        py: Python<'py>,
        name: String,
        value: Option<String>,
        publish_for: Option<f64>,
        namespace: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request = build_event_set(name, value, publish_for, namespace)?;
        let cell = Arc::clone(&self.cell);
        future_into_py(py, async move {
            let client = cell.get().await.map_err(err)?;
            client.set_event(request).await.map(set_tuple).map_err(err)
        })
    }

    #[pyo3(signature = (name, value, publish_for, namespace))]
    fn set_event_sync(
        &self,
        py: Python<'_>,
        name: String,
        value: Option<String>,
        publish_for: Option<f64>,
        namespace: Option<String>,
    ) -> PyResult<SetTuple> {
        let request = build_event_set(name, value, publish_for, namespace)?;
        let cell = Arc::clone(&self.cell);
        let result = block_on(
            py,
            async move { cell.get().await?.set_event(request).await },
        )?;
        result.map(set_tuple).map_err(err)
    }

    /// End the session on the server (releasing every lock) and stop the pool.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cell = Arc::clone(&self.cell);
        future_into_py(py, async move {
            match cell.client.get() {
                Some(client) => client.close().await.map_err(err),
                None => Ok(()),
            }
        })
    }

    fn close_sync(&self, py: Python<'_>) -> PyResult<()> {
        let cell = Arc::clone(&self.cell);
        block_on(py, async move {
            match cell.client.get() {
                Some(client) => client.close().await,
                None => Ok(()),
            }
        })?
        .map_err(err)
    }
}

// ------------------------------------------------------------------------------------ lock

#[pyclass(frozen, module = "distlock._native")]
struct NativeLock {
    /// Taken (leaving `None`) by `release`, which consumes the Rust lock.
    lock: Arc<tokio::sync::Mutex<Option<Lock>>>,
    lost: Lost,
    name: String,
    kind: &'static str,
    token: u64,
    ttl: f64,
    previous: Option<(&'static str, u64, f64)>,
}

impl NativeLock {
    fn new(lock: Lock) -> Self {
        Self {
            lost: lock.watch_lost(),
            name: lock.name().to_owned(),
            kind: kind_name(lock.kind()),
            token: lock.fencing_token(),
            ttl: lock.ttl().as_secs_f64(),
            previous: lock
                .previous()
                .map(|p| (state_name(p.state), p.set_by_token, p.age.as_secs_f64())),
            lock: Arc::new(tokio::sync::Mutex::new(Some(lock))),
        }
    }
}

async fn release(
    slot: Arc<tokio::sync::Mutex<Option<Lock>>>,
    state: Option<State>,
    state_ttl: Option<Duration>,
) -> Result<(), Error> {
    let lock = slot.lock().await.take();
    match lock {
        Some(lock) => lock.release_with(state, state_ttl).await,
        None => Ok(()), // already released: releasing twice is harmless
    }
}

async fn extend(slot: Arc<tokio::sync::Mutex<Option<Lock>>>, ttl: Duration) -> Result<f64, Error> {
    match slot.lock().await.as_ref() {
        Some(lock) => lock.extend(ttl).await.map(|d| d.as_secs_f64()),
        None => Err(Error::LeaseLost),
    }
}

#[pymethods]
impl NativeLock {
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    #[getter]
    fn kind(&self) -> &'static str {
        self.kind
    }

    #[getter]
    fn fencing_token(&self) -> u64 {
        self.token
    }

    /// Lease length the server granted, in seconds.
    #[getter]
    fn ttl(&self) -> f64 {
        self.ttl
    }

    /// `(status, set_by_fencing_token, age_seconds)` of the previous holder's status, if any.
    #[getter]
    fn previous(&self) -> Option<(&'static str, u64, f64)> {
        self.previous
    }

    #[getter]
    fn is_lost(&self) -> bool {
        self.lost.is_lost()
    }

    fn wait_lost<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let lost = self.lost.clone();
        future_into_py(py, async move {
            lost.wait().await;
            Ok(())
        })
    }

    fn wait_lost_sync(&self, py: Python<'_>) -> PyResult<()> {
        let lost = self.lost.clone();
        block_on(py, async move { lost.wait().await })
    }

    #[pyo3(signature = (state=None, state_ttl=None))]
    fn release<'py>(
        &self,
        py: Python<'py>,
        state: Option<&str>,
        state_ttl: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let state = state.map(parse_state).transpose()?;
        let state_ttl = opt_secs("state_ttl", state_ttl)?;
        let slot = Arc::clone(&self.lock);
        future_into_py(py, async move {
            release(slot, state, state_ttl).await.map_err(err)
        })
    }

    #[pyo3(signature = (state=None, state_ttl=None))]
    fn release_sync(
        &self,
        py: Python<'_>,
        state: Option<&str>,
        state_ttl: Option<f64>,
    ) -> PyResult<()> {
        let state = state.map(parse_state).transpose()?;
        let state_ttl = opt_secs("state_ttl", state_ttl)?;
        let slot = Arc::clone(&self.lock);
        block_on(py, release(slot, state, state_ttl))?.map_err(err)
    }

    fn extend<'py>(&self, py: Python<'py>, ttl: f64) -> PyResult<Bound<'py, PyAny>> {
        let ttl = secs("ttl", ttl)?;
        let slot = Arc::clone(&self.lock);
        future_into_py(py, async move { extend(slot, ttl).await.map_err(err) })
    }

    fn extend_sync(&self, py: Python<'_>, ttl: f64) -> PyResult<f64> {
        let ttl = secs("ttl", ttl)?;
        let slot = Arc::clone(&self.lock);
        block_on(py, extend(slot, ttl))?.map_err(err)
    }

    fn __repr__(&self) -> String {
        format!(
            "NativeLock(name={:?}, kind={}, fencing_token={}, lost={})",
            self.name,
            self.kind,
            self.token,
            self.lost.is_lost()
        )
    }
}

// ------------------------------------------------------------------------------------ module

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Two worker threads are plenty: the runtime only moves bytes and runs timers.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .worker_threads(2)
        .thread_name("distlock")
        .enable_all();
    pyo3_async_runtimes::tokio::init(builder);

    m.add_class::<NativeClient>()?;
    m.add_class::<NativeLock>()?;
    m.add("NativeError", m.py().get_type::<NativeError>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
