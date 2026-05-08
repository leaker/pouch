//! Config-driven ignoreUrls blacklist applied to the **original** URL after
//! `url_resolver::resolve`. Matched URLs bypass the cache and stream straight
//! through `http_fetcher`.
//!
//! Rules are loaded from `hook.config.json`'s `ignore_urls` field via
//! [`set_matchers`]; this module ships **no** built-in defaults. If the field
//! is missing, empty, or [`set_matchers`] is never called, [`is_ignored`]
//! simply returns `false` for every URL.
//!
//! [`set_matchers`] is an *atomic replace* — calling it again at runtime (the
//! Reload code path swaps in a freshly-compiled rule set) overwrites the
//! previous matcher list under a short write-lock window without restarting
//! the process.
//!
//! # Schema
//!
//! Each entry is **one** of four shapes — the field name carries the semantic
//! tag (no magic prefix strings):
//!
//! | Field            | Match domain | Semantics                                                                          |
//! |------------------|--------------|------------------------------------------------------------------------------------|
//! | `suffix`         | host         | Host suffix, includes the apex itself plus any subdomain (`gstatic.com` matches `gstatic.com` and `fonts.gstatic.com`). |
//! | `wildcard`       | host         | Host glob; `*` matches a single label segment (does **not** cross `.`). `*.google.com` matches `fonts.google.com` but not `google.com`. |
//! | `url_wildcard`   | full URL     | URL glob; `*` matches non-`/` runs (does **not** cross `/`). All other regex meta is escaped. Auto-anchored.       |
//! | `url_regex`      | full URL     | Raw `regex::Regex` against the full URL. **Not** auto-anchored — caller controls `^` / `$`.                        |
//!
//! `comment` is documentation-only and unused at runtime. Host comparisons
//! parse the URL via the `url` crate so scheme / port / path / userinfo / IPv6
//! brackets are handled correctly.

use std::sync::RwLock;

use regex::Regex;
use serde::Deserialize;
use tracing::warn;

/// On-disk schema for a single entry in `hook.config.json`'s `ignore_urls`
/// array. Untagged: the present field name selects the variant. Each variant
/// uses `deny_unknown_fields` so an entry that mixes variant keys (e.g.
/// `{ "suffix": "...", "wildcard": "..." }`) fails to match any single variant
/// and the whole `ignore_urls` value falls through to the loader's warn-and-
/// drop path. `comment` is schema-only and never consulted at runtime.
#[derive(Debug, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum IgnoreEntry {
    /// Host suffix, includes the apex (`gstatic.com` matches `gstatic.com`
    /// **and** `fonts.gstatic.com`).
    Suffix {
        suffix: String,
        #[serde(default)]
        #[allow(dead_code)] // schema-only field; documents intent in the JSON file
        comment: Option<String>,
    },
    /// Host glob; `*` matches a single label segment and does **not** cross `.`.
    Wildcard {
        wildcard: String,
        #[serde(default)]
        #[allow(dead_code)]
        comment: Option<String>,
    },
    /// URL glob; `*` matches non-`/` runs. Auto-anchored.
    UrlWildcard {
        url_wildcard: String,
        #[serde(default)]
        #[allow(dead_code)]
        comment: Option<String>,
    },
    /// Full-URL `regex::Regex`. **Not** auto-anchored.
    UrlRegex {
        url_regex: String,
        #[serde(default)]
        #[allow(dead_code)]
        comment: Option<String>,
    },
}

/// Compiled runtime form of a single ignore rule. Built once at startup via
/// [`compile_entries`].
#[derive(Debug)]
pub enum IgnoreMatcher {
    /// `suffix` — host equals the suffix, or ends with `.<suffix>` (apex
    /// included). Stored as the lowercased host string; cheaper than a regex.
    HostSuffix(String),
    /// `wildcard` — host glob compiled to a host-level regex (`*` = `[^.]*`).
    HostRegex(Regex),
    /// `url_wildcard` (auto-anchored, `*` = `[^/]*`) and `url_regex` (raw, no
    /// auto-anchor) both flatten into a full-URL regex.
    UrlRegex(Regex),
}

/// Process-wide compiled matchers, populated by [`set_matchers`]. Wrapped in a
/// `RwLock` so the Reload path can atomically replace the rule set at runtime
/// (`std::sync::RwLock` keeps zero new dependencies; readers hit the hot path
/// — `is_ignored` — and writers run only on startup / explicit reload, so
/// reader contention is essentially nil).
///
/// Stays empty (`Vec::new()`) until the first [`set_matchers`] call; readers
/// short-circuit on the empty vec.
static IGNORE_MATCHERS: RwLock<Vec<IgnoreMatcher>> = RwLock::new(Vec::new());

/// Convert a host glob (`*.google.com`, `ads.*.com`) into an anchored regex
/// where `*` only matches a single label segment (no `.`).
fn host_glob_to_regex(glob: &str) -> String {
    let mut out = String::with_capacity(glob.len() + 8);
    out.push('^');
    for ch in glob.chars() {
        match ch {
            '*' => out.push_str("[^.]*"),
            '.' | '\\' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('$');
    out
}

/// Convert a URL glob (`https://example.com/api/*`) into an anchored regex
/// where `*` matches a non-`/` run. All other regex meta is escaped.
fn url_glob_to_regex(glob: &str) -> String {
    let mut out = String::with_capacity(glob.len() + 8);
    out.push('^');
    for ch in glob.chars() {
        match ch {
            '*' => out.push_str("[^/]*"),
            '.' | '\\' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('$');
    out
}

/// Compile a list of [`IgnoreEntry`] into runtime matchers, logging and
/// skipping any individual entry that fails (empty pattern, invalid regex).
/// Always returns the successfully compiled subset; never errors out.
pub fn compile_entries(entries: &[IgnoreEntry]) -> Vec<IgnoreMatcher> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry {
            IgnoreEntry::Suffix { suffix, .. } => {
                let trimmed = suffix.trim();
                if trimmed.is_empty() {
                    warn!(
                        target: "hook",
                        "[ignore_filter] skipping ignore_urls entry with empty `suffix`"
                    );
                    continue;
                }
                out.push(IgnoreMatcher::HostSuffix(trimmed.to_ascii_lowercase()));
            }
            IgnoreEntry::Wildcard { wildcard, .. } => {
                let trimmed = wildcard.trim();
                if trimmed.is_empty() {
                    warn!(
                        target: "hook",
                        "[ignore_filter] skipping ignore_urls entry with empty `wildcard`"
                    );
                    continue;
                }
                let re_src = host_glob_to_regex(&trimmed.to_ascii_lowercase());
                match Regex::new(&re_src) {
                    Ok(re) => out.push(IgnoreMatcher::HostRegex(re)),
                    Err(e) => warn!(
                        target: "hook",
                        "[ignore_filter] skipping invalid host wildcard {:?}: {}",
                        trimmed, e
                    ),
                }
            }
            IgnoreEntry::UrlWildcard { url_wildcard, .. } => {
                let trimmed = url_wildcard.trim();
                if trimmed.is_empty() {
                    warn!(
                        target: "hook",
                        "[ignore_filter] skipping ignore_urls entry with empty `url_wildcard`"
                    );
                    continue;
                }
                let re_src = url_glob_to_regex(trimmed);
                match Regex::new(&re_src) {
                    Ok(re) => out.push(IgnoreMatcher::UrlRegex(re)),
                    Err(e) => warn!(
                        target: "hook",
                        "[ignore_filter] skipping invalid url_wildcard {:?}: {}",
                        trimmed, e
                    ),
                }
            }
            IgnoreEntry::UrlRegex { url_regex, .. } => {
                let trimmed = url_regex.trim();
                if trimmed.is_empty() {
                    warn!(
                        target: "hook",
                        "[ignore_filter] skipping ignore_urls entry with empty `url_regex`"
                    );
                    continue;
                }
                match Regex::new(trimmed) {
                    Ok(re) => out.push(IgnoreMatcher::UrlRegex(re)),
                    Err(e) => warn!(
                        target: "hook",
                        "[ignore_filter] skipping invalid url_regex {:?}: {}",
                        trimmed, e
                    ),
                }
            }
        }
    }
    out
}

/// Install (or replace) the compiled matcher set process-wide. Used both at
/// startup and by the runtime Reload path — calling twice atomically swaps in
/// the freshly-compiled rule set under a short write-lock window. A poisoned
/// lock (panic in another thread while holding the write guard, which the
/// reader never does) is logged and dropped — `is_ignored` already treats a
/// poisoned read as "no rules", so a one-off poison never escalates into a
/// process kill.
pub fn set_matchers(entries: &[IgnoreEntry]) {
    let compiled = compile_entries(entries);
    match IGNORE_MATCHERS.write() {
        Ok(mut guard) => {
            *guard = compiled;
        }
        Err(e) => warn!(
            target: "hook",
            "[ignore_filter] set_matchers: write lock poisoned: {e}"
        ),
    }
}

impl IgnoreMatcher {
    /// Test a single matcher against a URL. The caller passes a pre-parsed
    /// lowercased `host` so the hot path parses each URL at most once across
    /// the whole matcher list. `host` may be `None` if the URL failed to
    /// parse — host-domain matchers then return `false`.
    pub fn matches(&self, url: &str, host: Option<&str>) -> bool {
        match self {
            IgnoreMatcher::HostSuffix(suffix) => {
                let Some(h) = host else { return false };
                h == suffix
                    || (h.len() > suffix.len()
                        && h.ends_with(suffix.as_str())
                        && h.as_bytes()[h.len() - suffix.len() - 1] == b'.')
            }
            IgnoreMatcher::HostRegex(re) => host.is_some_and(|h| re.is_match(h)),
            IgnoreMatcher::UrlRegex(re) => re.is_match(url),
        }
    }
}

/// Returns `true` when `original_url` matches any installed ignore rule.
/// Returns `false` if no matchers were installed (config absent / empty), or
/// if the read lock is poisoned (fail-open — better to leak through one
/// request than to flap into "everything ignored" because a writer panicked).
pub fn is_ignored(original_url: &str) -> bool {
    let matchers = match IGNORE_MATCHERS.read() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if matchers.is_empty() {
        return false;
    }

    let host = url::Url::parse(original_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()));
    matchers
        .iter()
        .any(|m| m.matches(original_url, host.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile a single-entry JSON snippet to its sole matcher; panics on
    /// parse / compile failure so the test name surfaces the error directly.
    fn matcher(json: &str) -> IgnoreMatcher {
        let entries: Vec<IgnoreEntry> =
            serde_json::from_str(&format!("[{}]", json)).expect("parse entry");
        compile_entries(&entries)
            .into_iter()
            .next()
            .expect("compile produced no matcher")
    }

    fn host_of(url: &str) -> Option<String> {
        url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
    }

    fn hits(m: &IgnoreMatcher, url: &str) -> bool {
        m.matches(url, host_of(url).as_deref())
    }

    // --- suffix --------------------------------------------------------------

    #[test]
    fn suffix_matches_apex() {
        assert!(hits(
            &matcher(r#"{"suffix":"gstatic.com"}"#),
            "https://gstatic.com/x"
        ));
    }

    #[test]
    fn suffix_matches_subdomain() {
        assert!(hits(
            &matcher(r#"{"suffix":"gstatic.com"}"#),
            "https://fonts.gstatic.com/x"
        ));
    }

    #[test]
    fn suffix_rejects_substring_prefix() {
        assert!(!hits(
            &matcher(r#"{"suffix":"gstatic.com"}"#),
            "https://notgstatic.com/x"
        ));
    }

    #[test]
    fn suffix_rejects_evil_suffix() {
        assert!(!hits(
            &matcher(r#"{"suffix":"gstatic.com"}"#),
            "https://gstatic.com.evil/x"
        ));
    }

    #[test]
    fn suffix_matches_with_port() {
        assert!(hits(
            &matcher(r#"{"suffix":"gstatic.com"}"#),
            "https://gstatic.com:443/x"
        ));
    }

    // --- wildcard ------------------------------------------------------------

    #[test]
    fn wildcard_matches_subdomain() {
        assert!(hits(
            &matcher(r#"{"wildcard":"*.google.com"}"#),
            "https://x.google.com/y"
        ));
    }

    #[test]
    fn wildcard_strict_excludes_apex() {
        assert!(!hits(
            &matcher(r#"{"wildcard":"*.google.com"}"#),
            "https://google.com/y"
        ));
    }

    #[test]
    fn wildcard_middle_label() {
        assert!(hits(
            &matcher(r#"{"wildcard":"ads.*.com"}"#),
            "https://ads.foo.com/x"
        ));
    }

    #[test]
    fn wildcard_does_not_cross_dot() {
        assert!(!hits(
            &matcher(r#"{"wildcard":"ads.*.com"}"#),
            "https://ads.foo.bar.com/x"
        ));
    }

    // --- url_wildcard --------------------------------------------------------

    #[test]
    fn url_wildcard_matches_path_segment() {
        assert!(hits(
            &matcher(r#"{"url_wildcard":"https://example.com/api/*"}"#),
            "https://example.com/api/foo"
        ));
    }

    #[test]
    fn url_wildcard_does_not_cross_slash() {
        assert!(!hits(
            &matcher(r#"{"url_wildcard":"https://example.com/api/*"}"#),
            "https://example.com/api/foo/bar"
        ));
    }

    // --- url_regex -----------------------------------------------------------

    #[test]
    fn url_regex_matches_user_pattern() {
        assert!(hits(
            &matcher(r#"{"url_regex":"^https://example\\.com/track/.*"}"#),
            "https://example.com/track/abc"
        ));
    }
}
