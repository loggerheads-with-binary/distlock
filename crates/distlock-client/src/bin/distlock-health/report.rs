//! The figures a check reports, once, for the text output and for the webhook JSON.
use distlock_client::ServerStats;
use serde_json::{Value, json};

use crate::human;

/// How `--human-readable` writes a number. The JSON never uses it.
#[derive(Clone, Copy)]
pub enum Kind {
    /// Printed as it is: names, flags, ids, load averages.
    Plain,
    /// An integer quantity: `92_160`.
    Count,
    /// A size in bytes: `16.4 GiB`.
    Bytes,
    /// A percentage: `24.3%`.
    Percent,
}

pub struct Row {
    pub key: &'static str,
    /// `null` = not reported.
    pub value: Value,
    pub kind: Kind,
}

fn row(key: &'static str, value: Value, kind: Kind) -> Row {
    Row { key, value, kind }
}

pub fn rows(s: &ServerStats) -> Vec<Row> {
    use Kind::{Bytes, Count, Percent, Plain};
    let pct = |v: f64| json!((v * 10.0).round() / 10.0);
    let mut r = vec![
        row("version", json!(s.version), Plain),
        // Empty when the server predates the hash.
        row(
            "users_md5",
            if s.users_md5.is_empty() {
                Value::Null
            } else {
                json!(s.users_md5)
            },
            Plain,
        ),
        row("uptime_s", json!(s.uptime_ms / 1000), Count),
        row("ready", json!(!s.in_startup_grace), Plain),
        row(
            "startup_grace_remaining_ms",
            json!(s.startup_grace_remaining_ms),
            Count,
        ),
        row("connected_clients", json!(s.connected_clients), Count),
        row("connections", json!(s.connections), Count),
        row("sessions", json!(s.sessions), Count),
        row("locks_held", json!(s.locks_held), Count),
        row("requests_queued", json!(s.requests_queued), Count),
        row("lock_names", json!(s.lock_names), Count),
        row("events", json!(s.events), Count),
    ];
    if let Some(y) = &s.system {
        r.extend([
            row(
                "system_memory_total_bytes",
                json!(y.total_memory_bytes),
                Bytes,
            ),
            row(
                "system_memory_used_bytes",
                json!(y.used_memory_bytes),
                Bytes,
            ),
            row("system_cpu_count", json!(y.cpu_count), Count),
            row("system_cpu_percent", pct(y.cpu_percent), Percent),
            row("system_load_avg_1m", json!(y.load_avg_1m), Plain),
            row("system_load_avg_5m", json!(y.load_avg_5m), Plain),
            row("system_load_avg_15m", json!(y.load_avg_15m), Plain),
        ]);
    }
    if let Some(p) = &s.process {
        r.extend([
            row("process_pid", json!(p.pid), Plain),
            row("process_cpu_percent", pct(p.cpu_percent), Percent),
            row("process_memory_bytes", json!(p.memory_bytes), Bytes),
            row(
                "process_virtual_memory_bytes",
                json!(p.virtual_memory_bytes),
                Bytes,
            ),
            row("process_cpu_time_ms", json!(p.cpu_time_ms), Count),
            row("process_open_files", json!(p.open_files), Count),
            row("process_open_files_limit", json!(p.open_files_limit), Count),
        ]);
    }
    r
}

/// One value as text. With `human`, sizes, percentages and counts are made readable.
pub fn render(row: &Row, human: bool) -> String {
    match &row.value {
        Value::Null => "n/a".to_owned(),
        Value::String(t) => t.clone(),
        v if !human => v.to_string(),
        v => match (row.kind, v.as_u64(), v.as_f64()) {
            (Kind::Count, Some(n), _) => human::count(n),
            (Kind::Bytes, Some(n), _) => human::bytes(n),
            (Kind::Percent, _, Some(p)) => human::percent(p),
            _ => v.to_string(),
        },
    }
}

pub fn print(s: &ServerStats, human: bool) {
    for r in rows(s) {
        println!("{}: {}", r.key, render(&r, human));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(key: &'static str, value: Value, kind: Kind, human: bool) -> String {
        render(&row(key, value, kind), human)
    }

    #[test]
    fn human_mode_only_changes_numbers_of_the_right_kind() {
        assert_eq!(text("a", json!(92160), Kind::Count, true), "92_160");
        assert_eq!(text("a", json!(92160), Kind::Count, false), "92160");
        assert_eq!(text("a", json!(1536), Kind::Bytes, true), "1.5 KiB");
        assert_eq!(text("a", json!(1536), Kind::Bytes, false), "1536");
        assert_eq!(text("a", json!(24.3), Kind::Percent, true), "24.3%");
        assert_eq!(text("a", json!(24.3), Kind::Percent, false), "24.3");
        // A process id is a name, not a quantity.
        assert_eq!(text("a", json!(51911), Kind::Plain, true), "51911");
        assert_eq!(text("a", json!(true), Kind::Plain, true), "true");
        assert_eq!(text("a", json!(2.5), Kind::Plain, true), "2.5");
        assert_eq!(text("a", Value::Null, Kind::Bytes, true), "n/a");
        assert_eq!(text("a", json!("abc"), Kind::Count, true), "abc");
    }
}
