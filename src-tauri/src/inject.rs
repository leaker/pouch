//! URL-rule-based JavaScript injection.
//!
//! Scans an `inject/` directory for `.js` files with a Tampermonkey-style
//! frontmatter (`// ==UserScript== ... // ==/UserScript==`) and, at startup,
//! builds a single dispatcher script that is wired into the main webview via
//! [`tauri::webview::WebviewWindowBuilder::initialization_script`]. The
//! dispatcher decides per-rule, per-navigation, whether the current
//! `location.href` matches and runs the rule's body in an isolated function
//! scope.
//!
//! Design decisions (locked — do not relitigate here):
//! - Rules live as files in `inject/`, not in `hook.config.json`.
//! - `@match` is glob (`*` matches anything) or `regex:<pattern>` prefix.
//! - Missing `@match` ⇒ skip the file with a warning (no implicit "match all").
//! - Empty / missing `inject/` ⇒ caller short-circuits and never installs an
//!   initialization script (zero overhead).
//! - Per-rule `try { ... } catch { console.error }` lives in the dispatcher JS,
//!   not here, so a single bad rule cannot take out the others.
//! - `@grant` / `@require` / `@run-at` / `@noframes` / `@version` and other
//!   Tampermonkey extensions are intentionally **not** parsed; lines starting
//!   with `// @<word>` we don't recognise are silently ignored, which lets the
//!   user paste typical userscripts without us erroring out.
//!
//! Note on dynamic JS execution: the dispatcher runs `inject/*.js` bodies via
//! a `Function` constructor. This is deliberate — these files are part of
//! Pouch's user-supplied configuration surface and are explicitly trusted.

use std::path::PathBuf;

use tracing::{info, warn};

use crate::util::{pretty_path, user_data_dir, UserDataKind};

/// One injection rule, parsed from a single `inject/<name>.js` file.
#[derive(Debug, Clone)]
pub struct InjectRule {
    /// Human-readable name used in dispatcher error logs. Defaults to the
    /// file stem if `@name` is absent.
    pub name: String,
    /// At least one entry; files with zero patterns are dropped at scan time.
    pub patterns: Vec<MatchPattern>,
    /// The full file contents (frontmatter included). The dispatcher feeds
    /// this verbatim into a `Function` constructor; the frontmatter survives
    /// because it is `//` line comments which JS happily skips.
    pub code: String,
}

/// A single `@match` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchPattern {
    /// Simplified glob: `*` matches any character (including `/`); everything
    /// else is literal. Anchored at both ends in the dispatcher.
    Glob(String),
    /// Raw JavaScript-flavour regex source (no surrounding `/`); evaluated by
    /// `new RegExp(...)` in the dispatcher. Invalid patterns are caught at
    /// dispatch time, not here, so a bad regex doesn't break startup scan.
    Regex(String),
}

/// Scan the resolved `inject/` directory and return all parsed rules.
///
/// Rules are returned sorted by file name to give deterministic dispatch
/// order across runs and platforms (`read_dir` order is unspecified). An
/// empty result is the happy path for "no inject directory" / "no .js files"
/// / "no files survived parsing".
pub fn scan_inject_dir() -> Vec<InjectRule> {
    let Some(dir) = resolve_inject_dir() else {
        return Vec::new();
    };
    info!(target: "hook", "[inject] scanning {}", pretty_path(&dir).display());

    let mut entries: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().map(|x| x == "js").unwrap_or(false))
            .collect(),
        Err(e) => {
            warn!(target: "hook", "[inject] read_dir({}) failed: {}", pretty_path(&dir).display(), e);
            return Vec::new();
        }
    };
    entries.sort();

    let mut rules = Vec::new();
    for path in entries {
        match std::fs::read_to_string(&path) {
            Ok(content) => match parse_frontmatter(&content) {
                Some((name, patterns)) => {
                    let resolved_name = name.unwrap_or_else(|| {
                        path.file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "<unnamed>".into())
                    });
                    info!(
                        target: "hook",
                        "[inject] loaded rule {:?} ({} pattern(s)) from {}",
                        resolved_name,
                        patterns.len(),
                        pretty_path(&path).display()
                    );
                    rules.push(InjectRule {
                        name: resolved_name,
                        patterns,
                        code: content,
                    });
                }
                None => warn!(
                    target: "hook",
                    "[inject] skipped {}: no UserScript frontmatter or no @match entries",
                    pretty_path(&path).display()
                ),
            },
            Err(e) => warn!(
                target: "hook",
                "[inject] failed to read {}: {}",
                pretty_path(&path).display(),
                e
            ),
        }
    }
    rules
}

/// Build the single dispatcher JavaScript that the main webview should run as
/// its initialization script. Returns `None` when there are zero rules so the
/// caller can short-circuit (the brief explicitly forbids paying the
/// initialization-script cost when nothing would dispatch).
pub fn build_dispatcher_js(rules: &[InjectRule]) -> Option<String> {
    if rules.is_empty() {
        return None;
    }

    // Each `rules` entry is a JS object literal whose strings come straight
    // from `serde_json::to_string` — that gives us a guaranteed-valid JS
    // string literal for the body, the name, and each pattern, with all
    // backslash / quote / unicode / control-char escaping handled correctly.
    let mut entries = String::new();
    for rule in rules {
        let mut patterns_js = String::new();
        for p in &rule.patterns {
            let (kind, value) = match p {
                MatchPattern::Glob(v) => ("glob", v),
                MatchPattern::Regex(v) => ("regex", v),
            };
            patterns_js.push_str(&format!(
                "{{kind:{},value:{}}},",
                json_str(kind),
                json_str(value)
            ));
        }
        entries.push_str(&format!(
            "{{name:{},patterns:[{}],code:{}}},",
            json_str(&rule.name),
            patterns_js,
            json_str(&rule.code)
        ));
    }

    // Dispatcher template. Notes:
    // - Wrapped in IIFE + `'use strict'`.
    // - Glob escaping covers RegExp metachars; `*` is then re-introduced as
    //   `.*` (greedy — matches across `/` deliberately, which is the
    //   simpler-than-Tampermonkey semantics the brief picked).
    // - Each rule body executes in its own function scope built via the
    //   `Function` constructor; `'use strict'` from the dispatcher does NOT
    //   leak into the rule body unless the rule opts in itself.
    let js = format!(
        r#"(function(){{
'use strict';
var url=location.href;
function matchGlob(pattern,url){{
  var re=new RegExp('^'+pattern.replace(/[.+?^${{}}()|[\]\\]/g,'\\$&').replace(/\*/g,'.*')+'$');
  return re.test(url);
}}
function matchRegex(pattern,url){{
  try{{return new RegExp(pattern).test(url);}}
  catch(e){{console.error('[hook-inject] bad regex:',pattern,e);return false;}}
}}
var rules=[{entries}];
rules.forEach(function(rule){{
  var hit=rule.patterns.some(function(p){{
    return p.kind==='regex'?matchRegex(p.value,url):matchGlob(p.value,url);
  }});
  if(!hit) return;
  try{{(new Function(rule.code))();}}
  catch(e){{console.error('[hook-inject] rule "'+rule.name+'" failed:',e);}}
}});
}})();"#,
        entries = entries
    );
    Some(js)
}

/// Parse the Tampermonkey-style header. Returns `None` when there is no valid
/// `==UserScript== ... ==/UserScript==` block or no `@match` lines inside it.
///
/// Recognised lines (case-sensitive after the `// `):
/// - `// @name <text>` — optional, `<text>` trimmed.
/// - `// @match <pattern>` — multiple allowed; `regex:<...>` prefix recognised.
///
/// Unrecognised `// @foo ...` lines are skipped without warning so users can
/// paste real Tampermonkey scripts without having to strip them.
fn parse_frontmatter(content: &str) -> Option<(Option<String>, Vec<MatchPattern>)> {
    let mut in_block = false;
    let mut name: Option<String> = None;
    let mut patterns: Vec<MatchPattern> = Vec::new();

    for raw in content.lines() {
        let line = raw.trim_start();
        if !in_block {
            if line == "// ==UserScript==" {
                in_block = true;
            }
            continue;
        }
        if line == "// ==/UserScript==" {
            break;
        }
        // Inside the block we only care about `// @<key> <value>` shapes.
        let Some(rest) = line.strip_prefix("// @") else {
            continue;
        };
        let Some((key, value)) = rest.split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key {
            "name" => name = Some(value.to_string()),
            "match" => {
                if let Some(rx) = value.strip_prefix("regex:") {
                    patterns.push(MatchPattern::Regex(rx.to_string()));
                } else {
                    patterns.push(MatchPattern::Glob(value.to_string()));
                }
            }
            _ => {}
        }
    }

    if patterns.is_empty() {
        return None;
    }
    Some((name, patterns))
}

/// Resolve the `inject/` directory.
///
/// Resolution chain matches the rest of pouch (see
/// [`crate::util::user_data_dir`]):
/// - dev: `<CARGO_MANIFEST_DIR>/../inject`.
/// - macOS prod: `~/Library/Application Support/Pouch/inject`.
/// - Windows / Linux prod: `<exe parent>/inject`.
///
/// We additionally try `./inject` relative to cwd as a last-ditch fallback
/// (mirrors `config.rs`). Returns `None` when nothing exists; the caller
/// then falls into the zero-overhead "no rules" path.
fn resolve_inject_dir() -> Option<PathBuf> {
    if let Some(primary) = user_data_dir(UserDataKind::Inject) {
        if primary.is_dir() {
            return Some(primary);
        }
    }
    let cwd = PathBuf::from("inject");
    if cwd.is_dir() {
        return Some(cwd);
    }
    None
}

/// Wrap a string in a JavaScript string literal via `serde_json` so we get
/// correct escaping for backslash, quote, control chars, and non-BMP codepoints
/// without rolling our own table. JSON strings are a strict subset of valid JS
/// string literals, which is exactly what we need here.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).expect("serde_json::to_string on &str cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_frontmatter_minimal() {
        let src = "// ==UserScript==\n// @match https://example.com/*\n// ==/UserScript==\n";
        let (name, patterns) = parse_frontmatter(src).expect("should parse");
        assert!(name.is_none());
        assert_eq!(patterns.len(), 1);
        assert_eq!(
            patterns[0],
            MatchPattern::Glob("https://example.com/*".into())
        );
    }

    #[test]
    fn parse_frontmatter_with_name_and_multiple_match() {
        let src = "\
// ==UserScript==
// @name leelib
// @match https://www.leelib.com/*
// @match https://blog.leelib.com/*
// ==/UserScript==
console.log('body');
";
        let (name, patterns) = parse_frontmatter(src).expect("should parse");
        assert_eq!(name.as_deref(), Some("leelib"));
        assert_eq!(patterns.len(), 2);
    }

    #[test]
    fn parse_frontmatter_regex_prefix() {
        let src =
            "// ==UserScript==\n// @match regex:^https://(api|cdn)\\.x\\.com/\n// ==/UserScript==";
        let (_, patterns) = parse_frontmatter(src).expect("should parse");
        assert_eq!(
            patterns[0],
            MatchPattern::Regex("^https://(api|cdn)\\.x\\.com/".into())
        );
    }

    #[test]
    fn parse_frontmatter_ignores_unknown_fields() {
        let src = "\
// ==UserScript==
// @name demo
// @grant   none
// @run-at  document-start
// @version 1.0
// @match   https://example.com/*
// @noframes
// ==/UserScript==
";
        let (name, patterns) = parse_frontmatter(src).expect("should parse");
        assert_eq!(name.as_deref(), Some("demo"));
        assert_eq!(patterns.len(), 1);
    }

    #[test]
    fn parse_frontmatter_no_match_returns_none() {
        let src = "// ==UserScript==\n// @name lonely\n// ==/UserScript==";
        assert!(parse_frontmatter(src).is_none());
    }

    #[test]
    fn parse_frontmatter_no_block_returns_none() {
        assert!(parse_frontmatter("console.log('hi');").is_none());
        assert!(parse_frontmatter("// @match https://example.com/*\n").is_none());
    }

    #[test]
    fn parse_frontmatter_stops_at_end_marker() {
        // Anything after the closing marker is the rule body and must NOT
        // contribute @match lines (otherwise a `// @match` *inside the rule
        // code* would silently widen the rule).
        let src = "\
// ==UserScript==
// @match https://example.com/*
// ==/UserScript==
// @match https://malicious.example/*
";
        let (_, patterns) = parse_frontmatter(src).expect("should parse");
        assert_eq!(patterns.len(), 1);
        assert_eq!(
            patterns[0],
            MatchPattern::Glob("https://example.com/*".into())
        );
    }

    #[test]
    fn build_dispatcher_js_returns_none_for_empty_rules() {
        assert!(build_dispatcher_js(&[]).is_none());
    }

    #[test]
    fn build_dispatcher_js_embeds_strings_safely() {
        // Every gnarly thing we can throw at JSON literal escaping in one go:
        // backslashes, quotes, newlines, control chars, lone backtick, and
        // a non-BMP codepoint. If `serde_json::to_string` is doing its job
        // the dispatcher must remain syntactically valid.
        let rule = InjectRule {
            name: "weird\"name\\".into(),
            patterns: vec![MatchPattern::Glob("https://*/path?q=\"x\"".into())],
            code: "var s = \"line1\\n\"; // backtick`\nconsole.log('\u{1F600}');".into(),
        };
        let js = build_dispatcher_js(&[rule]).expect("should produce dispatcher");
        // Sanity: contains the IIFE and `Function` constructor invocation.
        assert!(js.contains("(function()"));
        assert!(js.contains("new Function(rule.code)"));
        // The raw quotes / backslashes should NOT appear unescaped inside
        // the embedded literals — they must show up as JSON escapes.
        assert!(js.contains("\\\""));
        assert!(js.contains("\\\\"));
        // Pattern kind labels render as JSON literals too.
        assert!(js.contains("\"glob\""));
    }

    #[test]
    fn scan_inject_dir_does_not_panic_and_rules_have_patterns() {
        // We don't assert empty (the repo's own inject/ may exist when this
        // crate's tests run) — only that it doesn't panic and that every
        // returned rule has at least one pattern.
        let v = scan_inject_dir();
        for rule in &v {
            assert!(
                !rule.patterns.is_empty(),
                "rule {:?} has no patterns",
                rule.name
            );
        }
    }

    #[test]
    fn parse_frontmatter_from_real_file_in_tempdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rule.js");
        let body = "\
// ==UserScript==
// @name tempdir-rule
// @match *
// ==/UserScript==
console.log('hi');
";
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let (name, patterns) = parse_frontmatter(&content).expect("should parse");
        assert_eq!(name.as_deref(), Some("tempdir-rule"));
        assert_eq!(patterns, vec![MatchPattern::Glob("*".into())]);
    }
}
