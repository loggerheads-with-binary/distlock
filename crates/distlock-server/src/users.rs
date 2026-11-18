//! The users file (`config/users.json`, schema in `config/users.schema.json`) and password checks.
//!
//! Login is bcrypt, and only for a session's *first* connection. Everything the schema cannot
//! express is enforced here: unique client ids, bcrypt cost, no unknown fields.
//!
//! # Lock-free swapping
//!
//! The users live in an immutable [`Snapshot`] behind an [`ArcSwap`]. A login check loads the
//! current snapshot (lock-free, never blocked by a writer) and uses it from start to finish, so it
//! always sees one complete set of users, never a half-applied update. [`UserDb::reload`] builds the
//! next snapshot entirely off to the side, including any slow hashing, and publishes it with a single
//! atomic pointer swap: readers pay nothing for it. The only lock in this module serialises
//! *reloads with each other*; no reader ever touches it. A reload that fails (unreadable or invalid
//! file) publishes nothing, so the previous users stay in force and checks keep working.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::util::{hex, random_bytes};

const MIN_COST: u32 = 10;
const MAX_CLIENT_ID_LEN: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum UsersError {
    #[error("cannot read users file {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("users file is not valid: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("users file is not valid: {0}")]
    Invalid(String),
    #[error(
        "these users were built in memory, not loaded from a file, so there is nothing to reload"
    )]
    NoSource,
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, UsersError> {
    Err(UsersError::Invalid(msg.into()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileFormat {
    #[serde(rename = "$schema")]
    _schema: Option<String>,
    #[serde(rename = "description")]
    _description: Option<String>,
    version: u32,
    users: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    client_id: String,
    password_hash: String,
    #[serde(default = "default_true")]
    enabled: bool,
    description: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug)]
struct Record {
    hash: Arc<str>,
    enabled: bool,
}

/// One immutable, internally consistent view of the users. Never modified after it is published.
#[derive(Debug)]
struct Snapshot {
    users: HashMap<String, Record>,
    /// Verified against when the user is unknown or disabled, so those logins cost the same CPU
    /// time as a wrong password for a real user. Cost = the highest cost in the file.
    dummy_hash: Arc<str>,
    dummy_cost: u32,
    /// The exact bytes this snapshot was parsed from, so reloading an unchanged file is a no-op.
    source_bytes: Option<Arc<[u8]>>,
    /// 1 for the first load, then +1 for every published reload.
    generation: u64,
}

impl Snapshot {
    /// `previous` lets the dummy hash be reused when its cost is unchanged (hashing is the slow
    /// part of building a snapshot).
    fn build(
        records: Vec<(String, String, bool)>,
        dummy_cost: u32,
        previous: Option<&Snapshot>,
        source_bytes: Option<Arc<[u8]>>,
    ) -> Self {
        let dummy_hash = match previous {
            Some(p) if p.dummy_cost == dummy_cost => Arc::clone(&p.dummy_hash),
            _ => {
                let password = hex(&random_bytes::<16>());
                bcrypt::hash(password, dummy_cost)
                    .expect("hashing a short password cannot fail")
                    .into()
            }
        };
        Self {
            users: records
                .into_iter()
                .map(|(id, hash, enabled)| {
                    (
                        id,
                        Record {
                            hash: hash.into(),
                            enabled,
                        },
                    )
                })
                .collect(),
            dummy_hash,
            dummy_cost,
            source_bytes,
            generation: previous.map_or(1, |p| p.generation + 1),
        }
    }

    fn parse(
        json: &str,
        previous: Option<&Snapshot>,
        source_bytes: Option<Arc<[u8]>>,
    ) -> Result<Self, UsersError> {
        let file: FileFormat = serde_json::from_str(json)?;
        if file.version != 1 {
            return invalid(format!("unsupported version {} (expected 1)", file.version));
        }
        if file.users.is_empty() {
            return invalid("`users` must contain at least one entry");
        }
        let mut seen = HashSet::new();
        let mut records = Vec::with_capacity(file.users.len());
        let mut dummy_cost = MIN_COST;
        for entry in file.users {
            validate_client_id(&entry.client_id)?;
            if !seen.insert(entry.client_id.clone()) {
                return invalid(format!("duplicate client_id `{}`", entry.client_id));
            }
            let Some(cost) = bcrypt_cost(&entry.password_hash) else {
                return invalid(format!(
                    "`{}`: password_hash is not a bcrypt hash of cost {MIN_COST} or more \
                     (create one with scripts/hash-password.sh)",
                    entry.client_id
                ));
            };
            if entry.description.as_ref().is_some_and(|d| d.len() > 500) {
                return invalid(format!(
                    "`{}`: description is longer than 500 bytes",
                    entry.client_id
                ));
            }
            dummy_cost = dummy_cost.max(cost);
            records.push((entry.client_id, entry.password_hash, entry.enabled));
        }
        Ok(Self::build(records, dummy_cost, previous, source_bytes))
    }
}

/// What a [`UserDb::reload`] changed. Names are for the server's own log; the wire protocol only
/// carries counts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reload {
    /// Generation of the users now in force.
    pub generation: u64,
    /// The file was byte-for-byte the same as the one in force: nothing was swapped.
    pub unchanged: bool,
    pub added: Vec<String>,
    /// No longer in the file.
    pub removed: Vec<String>,
    /// Still in the file but newly `enabled: false`.
    pub disabled: Vec<String>,
    /// Newly `enabled: true`.
    pub enabled: Vec<String>,
    pub password_changed: Vec<String>,
}

impl Reload {
    /// Users that can no longer log in and must be thrown out: removed, or newly disabled.
    /// (A changed password does not end live sessions.)
    pub fn revoked(&self) -> HashSet<String> {
        self.removed.iter().chain(&self.disabled).cloned().collect()
    }
}

fn diff(old: &Snapshot, new: &Snapshot) -> Reload {
    let mut r = Reload {
        generation: new.generation,
        ..Reload::default()
    };
    for (id, n) in &new.users {
        match old.users.get(id) {
            None => r.added.push(id.clone()),
            Some(o) => {
                if o.enabled && !n.enabled {
                    r.disabled.push(id.clone());
                }
                if !o.enabled && n.enabled {
                    r.enabled.push(id.clone());
                }
                if o.hash != n.hash {
                    r.password_changed.push(id.clone());
                }
            }
        }
    }
    r.removed = old
        .users
        .keys()
        .filter(|id| !new.users.contains_key(*id))
        .cloned()
        .collect();
    for list in [
        &mut r.added,
        &mut r.removed,
        &mut r.disabled,
        &mut r.enabled,
        &mut r.password_changed,
    ] {
        list.sort();
    }
    r
}

/// The users, swappable without locking readers, plus the machinery to check a password without
/// leaking which users exist.
#[derive(Debug)]
pub struct UserDb {
    current: ArcSwap<Snapshot>,
    source: Option<PathBuf>,
    /// Orders reloads against each other. Readers never take it.
    reload_lock: tokio::sync::Mutex<()>,
    /// Bounds concurrent bcrypt verifications (each is ~100-300 ms of CPU).
    limiter: Arc<Semaphore>,
}

impl UserDb {
    fn new(snapshot: Snapshot, source: Option<PathBuf>, max_concurrent: usize) -> Self {
        Self {
            current: ArcSwap::from_pointee(snapshot),
            source,
            reload_lock: tokio::sync::Mutex::new(()),
            limiter: Arc::new(Semaphore::new(max_concurrent.max(1))),
        }
    }

    /// Read and validate the users file. Blocks for about one bcrypt hash (the dummy hash).
    /// The path is remembered, so [`reload`](Self::reload) can read it again.
    pub fn load(path: &Path, max_concurrent_verifications: usize) -> Result<Self, UsersError> {
        let bytes = std::fs::read(path).map_err(|source| UsersError::Io {
            path: path.to_owned(),
            source,
        })?;
        warn_if_readable_by_others(path);
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| UsersError::Invalid(format!("not valid UTF-8: {e}")))?;
        let snapshot = Snapshot::parse(text, None, Some(Arc::from(bytes.as_slice())))?;
        Ok(Self::new(
            snapshot,
            Some(path.to_owned()),
            max_concurrent_verifications,
        ))
    }

    /// Users from JSON text. Not backed by a file, so it cannot be reloaded.
    pub fn parse(json: &str, max_concurrent_verifications: usize) -> Result<Self, UsersError> {
        let snapshot = Snapshot::parse(json, None, None)?;
        Ok(Self::new(snapshot, None, max_concurrent_verifications))
    }

    /// Fast in-memory users for tests: bcrypt cost 4, `(client_id, plaintext password, enabled)`.
    #[doc(hidden)]
    pub fn for_tests(users: &[(&str, &str, bool)]) -> Self {
        let records = users
            .iter()
            .map(|(id, pw, enabled)| {
                (
                    (*id).to_owned(),
                    bcrypt::hash(pw, 4).expect("hash"),
                    *enabled,
                )
            })
            .collect();
        Self::new(Snapshot::build(records, 4, None, None), None, 4)
    }

    pub fn user_count(&self) -> usize {
        self.current.load().users.len()
    }

    /// Generation of the users in force: 1 after the first load, +1 per published reload.
    pub fn generation(&self) -> u64 {
        self.current.load().generation
    }

    /// Whether `client_id` exists and may log in. Lock-free; reflects the users in force right now.
    pub fn is_enabled(&self, client_id: &str) -> bool {
        self.current
            .load()
            .users
            .get(client_id)
            .is_some_and(|r| r.enabled)
    }

    /// Re-read the users file and, if it changed and is valid, swap it in atomically.
    ///
    /// * Readers are never blocked and never see anything but a complete set of users.
    /// * On any failure nothing is published: the previous users stay in force.
    /// * An unchanged file is a cheap no-op (one file read, no hashing).
    /// * Concurrent reloads run one after another.
    pub async fn reload(&self) -> Result<Reload, UsersError> {
        let Some(path) = self.source.clone() else {
            return Err(UsersError::NoSource);
        };
        let _one_reload_at_a_time = self.reload_lock.lock().await;
        let old = self.current.load_full();
        let job = {
            let old = Arc::clone(&old);
            tokio::task::spawn_blocking(
                move || -> Result<Option<(Snapshot, Reload)>, UsersError> {
                    let bytes = std::fs::read(&path).map_err(|source| UsersError::Io {
                        path: path.clone(),
                        source,
                    })?;
                    if old.source_bytes.as_deref() == Some(bytes.as_slice()) {
                        return Ok(None);
                    }
                    warn_if_readable_by_others(&path);
                    let text = std::str::from_utf8(&bytes)
                        .map_err(|e| UsersError::Invalid(format!("not valid UTF-8: {e}")))?;
                    let new = Snapshot::parse(text, Some(&old), Some(Arc::from(bytes.as_slice())))?;
                    let report = diff(&old, &new);
                    Ok(Some((new, report)))
                },
            )
        };
        let outcome = job
            .await
            .map_err(|e| UsersError::Invalid(format!("the reload task failed: {e}")))??;
        match outcome {
            None => Ok(Reload {
                generation: old.generation,
                unchanged: true,
                ..Reload::default()
            }),
            Some((snapshot, report)) => {
                self.current.store(Arc::new(snapshot)); // the one atomic step readers can observe
                Ok(report)
            }
        }
    }

    /// Is this the right password for an enabled user? Unknown and disabled users run a full dummy
    /// verification and return `false`, indistinguishable from a wrong password.
    pub async fn verify(&self, client_id: &str, password: &str) -> bool {
        // One snapshot for the whole check, so a reload in the middle cannot mix old and new.
        let snapshot = self.current.load_full();
        let (hash, real) = match snapshot.users.get(client_id) {
            Some(r) if r.enabled => (Arc::clone(&r.hash), true),
            _ => (Arc::clone(&snapshot.dummy_hash), false),
        };
        let Ok(_permit) = self.limiter.acquire().await else {
            return false;
        };
        let password = password.to_owned();
        // A malformed hash or an over-long password makes bcrypt return an error: treat as a mismatch.
        let matched =
            tokio::task::spawn_blocking(move || bcrypt::verify(password, &hash).unwrap_or(false))
                .await
                .unwrap_or(false);
        matched && real
    }
}

fn validate_client_id(id: &str) -> Result<(), UsersError> {
    let mut chars = id.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !first_ok || !rest_ok || id.len() > MAX_CLIENT_ID_LEN {
        return invalid(format!(
            "client_id `{id}` must be 1-{MAX_CLIENT_ID_LEN} characters of [A-Za-z0-9._-] starting with a letter or digit"
        ));
    }
    Ok(())
}

/// The cost of a well-formed bcrypt hash (`$2a$`/`$2b$`/`$2y$`, 60 bytes) if it is at least `MIN_COST`.
fn bcrypt_cost(hash: &str) -> Option<u32> {
    let b = hash.as_bytes();
    let shape_ok = b.len() == 60
        && b[0] == b'$'
        && b[1] == b'2'
        && matches!(b[2], b'a' | b'b' | b'y')
        && b[3] == b'$'
        && b[6] == b'$'
        && b[7..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'/'));
    if !shape_ok || !b[4].is_ascii_digit() || !b[5].is_ascii_digit() {
        return None;
    }
    let cost = u32::from(b[4] - b'0') * 10 + u32::from(b[5] - b'0');
    (MIN_COST..=31).contains(&cost).then_some(cost)
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                file = %path.display(),
                mode = format_args!("{mode:o}"),
                "users file is readable by group or others; run `chmod 600`"
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const EXAMPLE: &str = include_str!("../../../config/users.example.json");

    /// A structurally valid cost-12 hash; nobody knows the password.
    fn some_hash() -> String {
        serde_json::from_str::<serde_json::Value>(EXAMPLE).unwrap()["users"][0]["password_hash"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn file(users: serde_json::Value) -> String {
        json!({ "version": 1, "users": users }).to_string()
    }

    fn rejected(json: &str) -> String {
        UserDb::parse(json, 1).unwrap_err().to_string()
    }

    #[test]
    fn the_shipped_example_file_loads() {
        let db = UserDb::parse(EXAMPLE, 1).unwrap();
        assert_eq!(db.user_count(), 3);
        assert!(db.is_enabled("billing-service"));
        assert!(
            !db.is_enabled("legacy-importer"),
            "disabled users cannot log in"
        );
        assert!(!db.is_enabled("nobody"));
        // Unknown/disabled users are verified against a dummy hash of the file's highest cost, so
        // that path takes as long as a real user's wrong password.
        let snapshot = db.current.load();
        assert!(
            snapshot.dummy_hash.starts_with("$2b$12$"),
            "{}",
            snapshot.dummy_hash
        );
    }

    #[test]
    fn invalid_files_are_rejected_with_a_reason() {
        let h = some_hash();
        let user = |extra: serde_json::Value| {
            let mut u = json!({ "client_id": "app", "password_hash": h });
            u.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            u
        };
        assert!(rejected(&file(json!([]))).contains("at least one"));
        assert!(
            rejected(&json!({"version": 2, "users": [user(json!({}))]}).to_string())
                .contains("version")
        );
        assert!(rejected(&file(json!([user(json!({})), user(json!({}))]))).contains("duplicate"));
        assert!(rejected(&file(json!([user(json!({"password": "x"}))]))).contains("unknown field"));
        assert!(
            rejected(&file(json!([user(json!({"client_id": "has space"}))]))).contains("client_id")
        );
        assert!(rejected(&file(json!([user(json!({"client_id": ""}))]))).contains("client_id"));
        assert!(
            rejected(&file(json!([user(json!({"password_hash": "hunter2"}))]))).contains("bcrypt")
        );
        let cost9 = format!("$2b$09${}", &h[7..]);
        assert!(rejected(&file(json!([user(json!({"password_hash": cost9}))]))).contains("cost"));
        assert!(
            rejected(&json!({"version": 1, "users": [user(json!({}))], "extra": 1}).to_string())
                .contains("unknown field")
        );
        assert!(rejected("not json").contains("not valid"));
    }

    #[test]
    fn accepted_hash_markers_and_costs() {
        let body = &some_hash()[7..];
        for ok in ["$2a$10$", "$2b$12$", "$2y$31$"] {
            assert!(bcrypt_cost(&format!("{ok}{body}")).is_some(), "{ok}");
        }
        for bad in ["$2c$12$", "$2b$09$", "$2b$32$", "$2b$1x$"] {
            assert!(bcrypt_cost(&format!("{bad}{body}")).is_none(), "{bad}");
        }
        assert!(bcrypt_cost(&some_hash()[..59]).is_none(), "wrong length");
    }

    #[tokio::test]
    async fn verify_accepts_only_the_right_password_of_an_enabled_user() {
        let db = UserDb::for_tests(&[
            ("billing", "correct horse", true),
            ("old", "legacy-pw", false),
        ]);
        assert!(db.verify("billing", "correct horse").await);
        assert!(!db.verify("billing", "correct horsf").await);
        assert!(!db.verify("billing", "").await);
        assert!(!db.verify("nobody", "correct horse").await, "unknown user");
        assert!(
            !db.verify("old", "legacy-pw").await,
            "disabled user, right password"
        );
    }

    #[tokio::test]
    async fn passwords_over_72_bytes_never_authenticate_and_never_panic() {
        let db = UserDb::for_tests(&[("billing", "pw", true)]);
        assert!(!db.verify("billing", &"x".repeat(500)).await);
    }

    #[test]
    fn load_reads_a_file_and_reports_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        std::fs::write(&path, EXAMPLE).unwrap();
        assert_eq!(UserDb::load(&path, 1).unwrap().user_count(), 3);
        let missing = UserDb::load(&dir.path().join("nope.json"), 1).unwrap_err();
        assert!(matches!(missing, UsersError::Io { .. }), "{missing}");
    }

    // ------------------------------------------------------------------ reloading

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    /// bcrypt hashes at the minimum accepted cost, computed once per password for the whole run.
    fn hash_of(password: &str, cost: u32) -> String {
        static CACHE: OnceLock<Mutex<HashMap<(String, u32), String>>> = OnceLock::new();
        let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
        cache
            .entry((password.to_owned(), cost))
            .or_insert_with(|| bcrypt::hash(password, cost).unwrap())
            .clone()
    }

    /// Write a users file: `(client_id, password, enabled)`.
    fn write_users(path: &Path, users: &[(&str, &str, bool)]) {
        let entries: Vec<_> = users
            .iter()
            .map(|(id, pw, enabled)| {
                json!({"client_id": id, "password_hash": hash_of(pw, 10), "enabled": enabled})
            })
            .collect();
        std::fs::write(path, json!({"version": 1, "users": entries}).to_string()).unwrap();
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    async fn reload_swaps_in_the_new_users_and_reports_exactly_what_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        write_users(
            &path,
            &[
                ("a", "pw-a", true),
                ("b", "pw-b", true),
                ("c", "pw-c", false),
            ],
        );
        let db = UserDb::load(&path, 4).unwrap();
        assert_eq!(db.generation(), 1);
        assert!(db.verify("a", "pw-a").await && db.verify("b", "pw-b").await);
        assert!(!db.verify("c", "pw-c").await, "c starts disabled");

        // a removed, b's password changed, c enabled, d added.
        write_users(
            &path,
            &[
                ("b", "pw-b2", true),
                ("c", "pw-c", true),
                ("d", "pw-d", true),
            ],
        );
        let r = db.reload().await.unwrap();
        assert_eq!(r.added, names(&["d"]));
        assert_eq!(r.removed, names(&["a"]));
        assert_eq!(r.password_changed, names(&["b"]));
        assert_eq!(r.enabled, names(&["c"]));
        assert!(r.disabled.is_empty() && !r.unchanged);
        assert_eq!(r.revoked(), HashSet::from(["a".to_owned()]));
        assert_eq!((r.generation, db.generation()), (2, 2));
        assert!(!db.verify("a", "pw-a").await, "removed");
        assert!(!db.verify("b", "pw-b").await, "old password");
        assert!(db.verify("b", "pw-b2").await);
        assert!(db.verify("c", "pw-c").await && db.verify("d", "pw-d").await);

        // Disabling is revocation too; a changed password alone is not.
        write_users(
            &path,
            &[
                ("b", "pw-b2", false),
                ("c", "pw-c", true),
                ("d", "pw-d2", true),
            ],
        );
        let r = db.reload().await.unwrap();
        assert_eq!(r.disabled, names(&["b"]));
        assert_eq!(r.password_changed, names(&["d"]));
        assert_eq!(
            r.revoked(),
            HashSet::from(["b".to_owned()]),
            "d keeps its sessions"
        );
    }

    #[tokio::test]
    async fn an_unchanged_file_is_a_no_op_and_a_cosmetic_change_changes_nothing_that_matters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        write_users(&path, &[("a", "pw-a", true)]);
        let db = UserDb::load(&path, 4).unwrap();

        let r = db.reload().await.unwrap();
        assert!(r.unchanged && r.revoked().is_empty());
        assert_eq!(
            (r.generation, db.generation()),
            (1, 1),
            "nothing was swapped"
        );

        // Same users, different bytes: swapped, but there is nothing to report.
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{text}\n")).unwrap();
        let r = db.reload().await.unwrap();
        assert!(!r.unchanged && r.revoked().is_empty());
        assert!(r.added.is_empty() && r.removed.is_empty() && r.password_changed.is_empty());
        assert_eq!(db.generation(), 2);
        assert!(db.verify("a", "pw-a").await);
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_loaded_leaves_the_current_users_in_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        write_users(&path, &[("a", "pw-a", true)]);
        let db = UserDb::load(&path, 4).unwrap();

        std::fs::write(&path, "{ this is not json").unwrap();
        assert!(db.reload().await.is_err());
        // Invalid but well-formed: duplicate ids.
        let one = json!({"client_id": "z", "password_hash": hash_of("pw-z", 10)});
        std::fs::write(&path, file(json!([one.clone(), one]))).unwrap();
        assert!(matches!(db.reload().await, Err(UsersError::Invalid(_))));
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(db.reload().await, Err(UsersError::Io { .. })));

        assert_eq!(db.generation(), 1, "no failed reload publishes anything");
        assert!(
            db.is_enabled("a") && db.verify("a", "pw-a").await,
            "checks keep working"
        );

        write_users(&path, &[("a", "pw-a", true), ("b", "pw-b", true)]);
        assert_eq!(db.reload().await.unwrap().added, names(&["b"]));
    }

    #[tokio::test]
    async fn users_built_in_memory_have_nothing_to_reload() {
        let db = UserDb::for_tests(&[("a", "pw", true)]);
        assert!(matches!(db.reload().await, Err(UsersError::NoSource)));
        assert!(db.verify("a", "pw").await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn checks_never_fail_while_the_users_are_swapped_underneath_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        let version = |n: usize| {
            // `stable` is in every version; the other user comes and goes and changes password.
            write_users(
                &path,
                &[
                    ("stable", "pw-stable", true),
                    (
                        "churn",
                        if n.is_multiple_of(2) {
                            "pw-c1"
                        } else {
                            "pw-c2"
                        },
                        !n.is_multiple_of(3),
                    ),
                ],
            );
        };
        version(0);
        let db = Arc::new(UserDb::load(&path, 4).unwrap());

        let stop = Arc::new(AtomicBool::new(false));
        let (checks, failures) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let verifiers: Vec<_> = (0..4)
            .map(|_| {
                let (db, stop, checks, failures) = (
                    Arc::clone(&db),
                    Arc::clone(&stop),
                    Arc::clone(&checks),
                    Arc::clone(&failures),
                );
                tokio::spawn(async move {
                    // Each check takes much longer than a swap, so swaps land in the middle of checks.
                    while !stop.load(Ordering::Relaxed) {
                        if !(db.is_enabled("stable") && db.verify("stable", "pw-stable").await) {
                            failures.fetch_add(1, Ordering::Relaxed);
                        }
                        checks.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();

        for n in 1..=150 {
            version(n);
            db.reload().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        stop.store(true, Ordering::Relaxed);
        for v in verifiers {
            v.await.unwrap();
        }
        assert_eq!(
            failures.load(Ordering::Relaxed),
            0,
            "a check failed while users were being swapped"
        );
        assert!(
            checks.load(Ordering::Relaxed) >= 4,
            "the checkers barely ran"
        );
        assert_eq!(db.generation(), 151);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn readers_are_not_held_up_by_a_slow_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        write_users(&path, &[("stable", "pw-stable", true)]);
        let db = Arc::new(UserDb::load(&path, 4).unwrap());

        // A higher bcrypt cost forces the reload to regenerate its dummy hash: the slowest thing a
        // reload can do. (Built before the timing starts.)
        let slow = json!([
            {"client_id": "stable", "password_hash": hash_of("pw-stable", 10)},
            {"client_id": "slow", "password_hash": hash_of("pw-slow", 12)},
        ]);
        std::fs::write(&path, file(slow)).unwrap();

        let started = std::time::Instant::now();
        let reloading = {
            let db = Arc::clone(&db);
            tokio::spawn(async move { db.reload().await })
        };
        let mut checks = 0u64;
        while !reloading.is_finished() {
            assert!(db.is_enabled("stable"));
            checks += 1;
            tokio::task::yield_now().await;
        }
        let reload_took = started.elapsed();
        let report = reloading.await.unwrap().unwrap();
        println!("{checks} lock-free checks ran during a {reload_took:?} reload");
        assert_eq!(report.added, names(&["slow"]));
        assert!(
            checks > 100,
            "only {checks} lock-free checks ran during a {reload_took:?} reload: readers were blocked"
        );
        assert!(db.verify("slow", "pw-slow").await);
    }
}
