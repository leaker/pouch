// ==UserScript==
// @name global hook injection demo
// @match *
// ==/UserScript==

/* -----------------------------------------------------------------------------
 * Demo script shipped with Pouch
 * -----------------------------------------------------------------------------
 * Demonstrates how a "global injection" rule works. Delete or replace it with
 * your own script at any time.
 *
 * How to disable / remove injection (pick the granularity you need):
 *   1. No injection at all → delete the entire `inject/` directory. Startup
 *      logs will print `inject rules = 0 (dispatcher will NOT be attached)`
 *      and there is zero runtime overhead.
 *   2. Drop just this rule → delete this .js file.
 *   3. Disable temporarily without deleting → comment out every `@match` line.
 *      A file with no `@match` will warn and be skipped at startup; other
 *      scripts are unaffected.
 *   4. Change the trigger conditions → edit `@match`:
 *        - glob form: use `*` as a wildcard (e.g. `@match https://*.example.com/*`)
 *        - regex form: prefix with `regex:` (e.g. `@match regex:^https://(a|b)\.com/`)
 *        - multiple `@match` lines are allowed; any single match triggers the rule
 *
 * A note on `@match *`:
 *   Pouch treats a bare `*` as a top-frame startup fallback so this demo does
 *   not run once for every iframe on the page. To target iframes, declare the
 *   iframe URL explicitly, for example `@match https://widget.example.com/*`.
 *   A safer broad web default is `@match https://*`, which only matches
 *   http(s) origins and skips special frames like `about:blank` or `data:`.
 *   Narrow the scope explicitly when writing your own scripts.
 *
 * Run timing and isolation:
 *   - Injected at document_start, before any of the page's own JS runs (same
 *     behaviour as Tampermonkey's `@run-at document-start`).
 *   - SPA route changes do NOT re-run this script. Hook the history API
 *     yourself if you need to react to client-side navigation.
 *   - Each rule is wrapped in its own try/catch by the dispatcher, so one
 *     script throwing does not break any others.
 *
 * Recommendations for your own scripts:
 *   - Wrap in an IIFE (as below) to avoid polluting the target page's global
 *     namespace.
 *   - The dispatcher already runs each rule in its own function scope, so the
 *     IIFE is not strictly required, but it is still good practice.
 * ---------------------------------------------------------------------------*/

(function() {
    console.log('[hook-inject][global] running on', location.host);
})();
