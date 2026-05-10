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

/// `CREATE TABLE IF NOT EXISTS` — single-column primary key, `WITHOUT ROWID`
/// so the host string IS the row identifier (no shadow rowid).
fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS learned_hosts (host TEXT PRIMARY KEY) WITHOUT ROWID",
        [],
    )?;
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

/// Add `host` to the learned set. Returns `true` if the entry was new (not
/// already present). New entries trigger a single `INSERT OR IGNORE` plus an
/// info log line.
pub fn learn_passthrough(host: &str) -> bool {
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
    tracing::info!(target: "hook", "[mitm] learned passthrough host={host}");
    drop(guard);
    persist_one(&host);
    true
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

/// Insert a single learned host into the on-disk store. Errors are logged
/// but never surfaced — losing one persisted entry is strictly less bad than
/// crashing the proxy; the in-memory `HashSet` keeps working regardless.
fn persist_one(host: &str) {
    let conn = match open_conn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "hook", "[mitm] persist learned: open: {e}");
            return;
        }
    };
    if let Err(e) = conn.execute(
        "INSERT OR IGNORE INTO learned_hosts (host) VALUES (?1)",
        [host],
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
        // Walk parent span scope newest-first; the closest span carrying a
        // `uri` field is the one we want (CONNECT span for tunnel-level
        // failures, GET span for upgrade_websocket failures — both contain
        // host:port that we should learn).
        let Some(span) = ctx.event_span(event) else {
            return;
        };
        for s in span.scope() {
            let ext = s.extensions();
            if let Some(uri) = ext.get::<SpanUri>() {
                if let Some(host) = extract_host(&uri.0) {
                    let _ = learn_passthrough(&host);
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
}
