//! Self-learning passthrough: when hudsucker emits an upstream TLS / connect
//! ERROR for a host (cert pinning, legacy TLS 1.0/1.1 + RSA-KX, handshake
//! failures rustls won't speak), record the host so the next CONNECT
//! to it bypasses MITM and is byte-forwarded instead. Persisted across
//! launches at `~/Library/Application Support/Pouch/cache/learned_passthrough.db`.
//!
//! The hook side is a `tracing_subscriber::Layer` ([`LearnerLayer`]) — hudsucker
//! 0.24 has no user-facing error callback on its handler traits, so we tap
//! the only signal it does emit: ERROR-level tracing events under spans whose
//! `uri` field carries the offending `host:port`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::RwLock;

use rusqlite::Connection;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Hosts (lowercase, no port) we've decided to passthrough. Populated from
/// disk on startup via [`load_learned_passthrough`] and grown at runtime by
/// [`learn_passthrough`].
static LEARNED_PASSTHROUGH: RwLock<Option<HashSet<String>>> = RwLock::new(None);

fn ensure_init(set: &mut Option<HashSet<String>>) -> &mut HashSet<String> {
    set.get_or_insert_with(HashSet::new)
}

fn store_path() -> Option<PathBuf> {
    let root = crate::util::macos_app_support_dir()?;
    Some(root.join("cache").join("learned_passthrough.db"))
}

/// `CREATE TABLE IF NOT EXISTS` — primary key on host (`WITHOUT ROWID`), with
/// `learned_at` (unix epoch seconds) and `reason` (truncated error message)
/// for post-hoc inspection / cleanup. Old single-column DBs are migrated via
/// `ALTER TABLE ADD COLUMN` so existing learned hosts survive the upgrade.
fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS learned_hosts (
            host TEXT PRIMARY KEY,
            learned_at INTEGER NOT NULL DEFAULT 0,
            reason TEXT NOT NULL DEFAULT ''
        ) WITHOUT ROWID",
        [],
    )?;

    // Migrate legacy single-column schema → 3-column.
    let col_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('learned_hosts')",
        [],
        |row| row.get(0),
    )?;
    if col_count == 1 {
        conn.execute(
            "ALTER TABLE learned_hosts ADD COLUMN learned_at INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        conn.execute(
            "ALTER TABLE learned_hosts ADD COLUMN reason TEXT NOT NULL DEFAULT ''",
            [],
        )?;
        tracing::info!(
            target: "hook",
            "[mitm] migrated learned_passthrough.db schema (added learned_at + reason)"
        );
    }

    Ok(())
}

/// Open (or create) the SQLite file, creating the parent dir if needed.
fn open_conn() -> rusqlite::Result<Connection> {
    let path = store_path().ok_or_else(|| {
        rusqlite::Error::InvalidPath(PathBuf::from("learned_passthrough.db"))
    })?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(&path)?;
    init_schema(&conn)?;
    Ok(conn)
}

/// Strip optional `:port` and lower-case. Bracketed IPv6 (`[::1]:443`) keeps
/// its brackets — the matcher always normalises through the same helper.
fn normalise_host(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    // Bracketed IPv6 literal: take everything up to and including ']'.
    if let Some(stripped) = s.strip_prefix('[') {
        let end = stripped.find(']')?;
        return Some(format!("[{}]", &stripped[..end]).to_ascii_lowercase());
    }
    // Plain host[:port]
    let host = s.split(':').next().unwrap_or(s);
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Add `host` to the learned set, tagging it with the (truncated) error
/// `reason` that triggered learning. Returns `true` if the entry was new.
/// New entries trigger a single `INSERT OR IGNORE` plus an info log line.
pub fn learn_passthrough(host: &str, reason: &str) -> bool {
    let Some(host) = normalise_host(host) else {
        return false;
    };

    let mut guard = match LEARNED_PASSTHROUGH.write() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(target: "hook", "[mitm] learn_passthrough: poisoned write lock: {e}");
            return false;
        }
    };
    let set = ensure_init(&mut guard);
    if !set.insert(host.clone()) {
        return false;
    }
    let reason_short = truncate_reason_owned(reason);
    tracing::info!(
        target: "hook",
        "[mitm] learned passthrough host={host} reason={reason_short}"
    );
    drop(guard);
    persist_one(&host, &reason_short);
    true
}

/// Truncate `s` to at most `MAX` bytes, snapping back to the nearest UTF-8
/// char boundary so we never split a codepoint mid-byte.
fn truncate_reason_owned(s: &str) -> String {
    const MAX: usize = 200;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Returns true if `authority` (`host:port`) is in the learned passthrough
/// set. Fail-open on a poisoned lock or absent state.
pub fn is_learned_passthrough(authority: &str) -> bool {
    let Some(host) = normalise_host(authority) else {
        return false;
    };
    let guard = match LEARNED_PASSTHROUGH.read() {
        Ok(g) => g,
        Err(_) => return false,
    };
    guard.as_ref().is_some_and(|set| set.contains(&host))
}

/// Read the on-disk SQLite store into [`LEARNED_PASSTHROUGH`]. Missing file
/// is not an error — a fresh DB is created on first open and yields an empty
/// set. Any SQLite failure is logged and treated as empty so a corrupt /
/// unreachable file never blocks startup.
pub fn load_learned_passthrough() -> std::io::Result<usize> {
    let conn = match open_conn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                target: "hook",
                "[mitm] learned_passthrough open failed: {e}; treating as empty"
            );
            return Ok(0);
        }
    };
    let mut set = HashSet::new();
    let load = (|| -> rusqlite::Result<()> {
        let mut stmt = conn.prepare("SELECT host FROM learned_hosts")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            let h = row?;
            if let Some(n) = normalise_host(&h) {
                set.insert(n);
            }
        }
        Ok(())
    })();
    if let Err(e) = load {
        tracing::warn!(
            target: "hook",
            "[mitm] learned_passthrough query failed: {e}; treating as empty"
        );
        return Ok(0);
    }
    let n = set.len();
    if let Ok(mut guard) = LEARNED_PASSTHROUGH.write() {
        *guard = Some(set);
    }
    Ok(n)
}

/// Insert a single learned host (with timestamp + truncated reason) into the
/// on-disk store. Errors are logged but never surfaced — losing one persisted
/// entry is strictly less bad than crashing the proxy; the in-memory
/// `HashSet` keeps working regardless.
fn persist_one(host: &str, reason: &str) {
    let conn = match open_conn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "hook", "[mitm] persist learned: open: {e}");
            return;
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if let Err(e) = conn.execute(
        "INSERT OR IGNORE INTO learned_hosts (host, learned_at, reason) VALUES (?1, ?2, ?3)",
        rusqlite::params![host, now, reason],
    ) {
        tracing::warn!(target: "hook", "[mitm] persist learned: insert: {e}");
    }
}

/// Span-extension wrapper so `on_event` can recover the originating CONNECT
/// span's `uri` field (set in `on_new_span` via [`UriVisitor`]).
#[derive(Clone)]
struct SpanUri(String);

struct UriVisitor(Option<String>);

impl Visit for UriVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "uri" {
            self.0 = Some(value.to_string());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "uri" {
            // Debug-formatted URIs are wrapped in quotes — strip them so the
            // host extractor sees a clean `host:port` / `https://...` token.
            let mut s = format!("{value:?}");
            if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
                s = s[1..s.len() - 1].to_string();
            }
            self.0 = Some(s);
        }
    }
}

/// Visitor that concatenates `error` / `error.sources` / `message` fields from
/// a tracing event so we can pattern-match against the textual error chain.
#[derive(Default)]
struct ErrorVisitor(String);

impl ErrorVisitor {
    fn matches(name: &str) -> bool {
        name == "error" || name == "message" || name.starts_with("error.")
    }
    fn push(&mut self, s: &str) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(s);
    }
}

impl Visit for ErrorVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if Self::matches(field.name()) {
            self.push(value);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if Self::matches(field.name()) {
            self.push(&format!("{value:?}"));
        }
    }
}

/// Whitelist of error patterns where switching to passthrough is plausible:
/// legacy TLS handshakes, cert validation failures, unknown CAs. Transient
/// failures (connection reset / refused / timeout / EOF) are excluded — they
/// would learn good hosts and permanently break their MITM (cookie / cache).
fn is_learnable_error(msg: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "HandshakeFailure",
        "received fatal alert",
        "invalid peer certificate",
        "host name mismatch",
        "certificate not valid",
        "unknown CA",
        "unknown ca",
        "protocol_version",
    ];
    PATTERNS.iter().any(|p| msg.contains(p))
}

/// Pull a host out of either a hudsucker CONNECT span uri (`host:port`) or
/// the nested upgrade_websocket / serve_stream span (full URL). Returns
/// `None` for empty / unparseable input.
fn extract_host(uri: &str) -> Option<String> {
    let s = uri.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains("://") {
        // Full URL — let url crate parse it.
        if let Ok(u) = url::Url::parse(s) {
            if let Some(h) = u.host_str() {
                return Some(h.to_ascii_lowercase());
            }
        }
        return None;
    }
    normalise_host(s)
}

/// `tracing_subscriber::Layer` that watches for hudsucker ERROR events and
/// promotes the offending host into the learned-passthrough set. Registered
/// alongside the existing fmt layer in `lib.rs::init_tracing`.
pub struct LearnerLayer;

impl<S> Layer<S> for LearnerLayer
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = UriVisitor(None);
        attrs.record(&mut visitor);
        if let Some(uri) = visitor.0 {
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(SpanUri(uri));
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();

        // 防止 LearnerLayer 自己 emit 的诊断 log（target=hook）再绕回这里。
        if metadata.target() == "hook" {
            return;
        }

        // 诊断：确认 hudsucker ERROR event 真的被 dispatch 进来。
        if metadata.target().starts_with("hudsucker")
            && metadata.level() == &tracing::Level::ERROR
        {
            tracing::debug!(
                target: "hook",
                "[mitm] learner saw hudsucker error target={}",
                metadata.target()
            );
        }

        if metadata.level() != &tracing::Level::ERROR {
            return;
        }
        if !metadata.target().starts_with("hudsucker") {
            return;
        }

        // Filter out transient errors (connection reset / refused / timeout /
        // EOF) where passthrough wouldn't help — learning them would
        // permanently disable MITM on a perfectly good host.
        let mut err_visitor = ErrorVisitor::default();
        event.record(&mut err_visitor);
        if !is_learnable_error(&err_visitor.0) {
            tracing::trace!(
                target: "hook",
                "[mitm] skip learn (transient error): {}",
                err_visitor.0.chars().take(80).collect::<String>()
            );
            return;
        }

        // Walk parent span scope newest-first; the closest span carrying a
        // `uri` field is the one we want (CONNECT span for tunnel-level
        // failures, GET span for upgrade_websocket failures — both contain
        // host:port that we should learn).
        let Some(span) = ctx.event_span(event) else {
            return;
        };
        let reason = err_visitor.0.clone();
        for s in span.scope() {
            let ext = s.extensions();
            if let Some(uri) = ext.get::<SpanUri>() {
                if let Some(host) = extract_host(&uri.0) {
                    let _ = learn_passthrough(&host, &reason);
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_host_strips_port_and_lowercases() {
        assert_eq!(normalise_host("Example.COM:443").as_deref(), Some("example.com"));
        assert_eq!(normalise_host("host"), Some("host".into()));
        assert_eq!(normalise_host(""), None);
        assert_eq!(normalise_host("[::1]:8080").as_deref(), Some("[::1]"));
    }

    #[test]
    fn extract_host_handles_url_and_authority() {
        assert_eq!(
            extract_host("https://gs152.tzcq.longget.com:11013/").as_deref(),
            Some("gs152.tzcq.longget.com")
        );
        assert_eq!(
            extract_host("setcookie.net:443").as_deref(),
            Some("setcookie.net")
        );
        assert_eq!(extract_host(""), None);
    }

    #[test]
    fn learnable_error_recognition() {
        // Should learn: TLS handshake / cert / CA failures where passthrough helps.
        assert!(is_learnable_error("received fatal alert: HandshakeFailure"));
        assert!(is_learnable_error(
            "invalid peer certificate: certificate not valid for name 'foo'"
        ));
        assert!(is_learnable_error("A host name mismatch has occurred."));
        assert!(is_learnable_error(
            "rustls: invalid peer certificate: unknown CA"
        ));
        assert!(is_learnable_error(
            "received fatal alert: protocol_version"
        ));

        // Should NOT learn: transient network errors, passthrough won't fix them.
        assert!(!is_learnable_error(
            "connection error error.sources=[connection reset]"
        ));
        assert!(!is_learnable_error("timed out"));
        assert!(!is_learnable_error("connection refused"));
        assert!(!is_learnable_error("unexpected end of file"));
        assert!(!is_learnable_error(""));
    }

    #[test]
    fn truncate_reason_respects_char_boundary() {
        // ASCII < MAX: unchanged.
        assert_eq!(truncate_reason_owned("hello"), "hello");

        // ASCII exactly at MAX: unchanged.
        let exact = "a".repeat(200);
        assert_eq!(truncate_reason_owned(&exact), exact);

        // ASCII > MAX: truncated to 200 bytes.
        let long = "a".repeat(500);
        let out = truncate_reason_owned(&long);
        assert_eq!(out.len(), 200);

        // Multi-byte UTF-8: must not split mid-codepoint.
        // '中' = 3 bytes; 80 chars = 240 bytes > 200. Truncation must still
        // produce valid UTF-8 (no partial code units).
        let zh = "中".repeat(80);
        let out = truncate_reason_owned(&zh);
        assert!(out.len() <= 200);
        assert!(out.is_char_boundary(out.len()));
        // All chars survived intact (every kept byte triple → one '中').
        assert!(out.chars().all(|c| c == '中'));

        // Emoji (4-byte UTF-8) at the boundary.
        let emoji = "🦀".repeat(60); // 240 bytes
        let out = truncate_reason_owned(&emoji);
        assert!(out.len() <= 200);
        assert!(out.is_char_boundary(out.len()));
        assert!(out.chars().all(|c| c == '🦀'));
    }

    #[test]
    fn schema_migration_from_legacy() {
        // Simulate a pre-upgrade DB: single-column legacy schema.
        let conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute(
            "CREATE TABLE learned_hosts (host TEXT PRIMARY KEY) WITHOUT ROWID",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO learned_hosts (host) VALUES ('legacy.example.com')",
            [],
        )
        .unwrap();

        // Run migration.
        init_schema(&conn).expect("migrate");

        // Verify schema is now 3 columns.
        let col_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('learned_hosts')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(col_count, 3);

        // Existing row preserved with default learned_at=0, reason=''.
        let (host, learned_at, reason): (String, i64, String) = conn
            .query_row(
                "SELECT host, learned_at, reason FROM learned_hosts WHERE host = 'legacy.example.com'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(host, "legacy.example.com");
        assert_eq!(learned_at, 0);
        assert_eq!(reason, "");

        // Idempotent: running again on already-migrated schema is a no-op.
        init_schema(&conn).expect("idempotent migrate");
        let col_count2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('learned_hosts')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(col_count2, 3);
    }
}
