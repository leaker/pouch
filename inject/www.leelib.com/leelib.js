// ==UserScript==
// @name leelib custom banner
// @match https://www.leelib.com/*
// ==/UserScript==

/* -----------------------------------------------------------------------------
 * Demo script shipped with Pouch
 * -----------------------------------------------------------------------------
 * A "banner injection" example: pins a green strip to the top of the target
 * page so you can see at a glance that injection is actually wired up. Real
 * scripts more typically do things like:
 *   - inject a dark-mode / custom theme CSS
 *   - add custom keyboard shortcuts (keydown listeners)
 *   - auto-login / auto-fill forms
 *   - tweak, hide, or remove specific DOM elements
 *   - data export / Tampermonkey-style page enhancements
 *
 * How to disable / remove injection (pick the granularity you need):
 *   1. No injection at all → delete the entire `inject/` directory. Startup
 *      logs will print `inject rules = 0 (dispatcher will NOT be attached)`
 *      and there is zero runtime overhead.
 *   2. Drop just this banner → delete this file.
 *   3. Disable temporarily without deleting → comment out every `@match` line.
 *      A file with no `@match` will warn and be skipped at startup; other
 *      scripts are unaffected.
 *   4. Change which site triggers it → edit the `@match` above:
 *        - glob form: `@match https://*.example.com/*`
 *        - regex form: prefix with `regex:`, e.g. `@match regex:^https://shop\.`
 *        - multiple `@match` lines are allowed; any single match triggers the rule
 *
 * Run timing and isolation:
 *   - Injected at document_start, before any of the page's own JS runs (same
 *     behaviour as Tampermonkey's `@run-at document-start`). That's why the
 *     code below first checks whether `document.body` already exists and
 *     either runs immediately or waits for DOMContentLoaded.
 *   - SPA route changes do NOT re-run this script. Hook the history API
 *     yourself if you need to react to client-side navigation.
 *   - Each rule is wrapped in its own try/catch by the dispatcher, so one
 *     script throwing does not break any others.
 *
 * Recommendation: wrap your own scripts in an IIFE (as below) to avoid
 * polluting the target page's global namespace.
 * ---------------------------------------------------------------------------*/

(function() {
    if (document.body) injectBanner();
    else document.addEventListener('DOMContentLoaded', injectBanner);
    function injectBanner() {
        const el = document.createElement('div');
        el.textContent = '[hook-inject] hello from Pouch';
        el.style.cssText = 'position:fixed;top:0;left:0;right:0;background:#10b981;color:white;padding:6px 12px;font:13px/1.4 system-ui;z-index:99999;text-align:center';
        document.body.appendChild(el);
    }
})();
