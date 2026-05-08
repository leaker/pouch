# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - 2026-05-08

First public release.

### Features

- macOS + Windows webview HTTP/HTTPS GET interception with on-disk caching to `overrides/<host>/<path>` plus colocated `<file>.meta.json` sidecar (preserves upstream `Content-Type` so cache hits don't guess from filename).
- Atomic cache writes via `tempfile::NamedTempFile + persist` (POSIX `rename(2)`); never leaves half-written bodies or sidecars on disk.
- Query-aware cache keys: `host/path.__qs_<8 hex>__<ext>` for URLs with query strings, so signed/dynamic URLs (`?time=…&sign=…`) no longer replay stale tokens across launches.
- Tampermonkey-style JavaScript injection via `inject/*.js` with `@match` URL globs and `@match regex:…` regex form. Rules run at `document_start` in their own function scope; one rule throwing does not affect others.
- Configurable `ignore_urls` blacklist supporting four entry shapes:
  - `suffix` — host suffix (apex + all subdomains)
  - `wildcard` — host glob (`*` does not cross `.`)
  - `url_wildcard` — full-URL glob (`*` does not cross `/`)
  - `url_regex` — full-URL regex (caller controls anchors)
- Window size config (`window` field): `"screen"` (default, fills work area), `"fullscreen"`, or fixed `{ "width", "height" }` logical pixels.
- `target_url` priority chain: CLI argv[1] → `TAURI_HOOK_TARGET_URL` env → `hook.config.json` → built-in default; each step falls through gracefully on failure.
- Cross-platform DevTools: `F12` opens DevTools on both macOS and Windows (via app menu accelerator; matches Chrome on both platforms).
- Native `document.title` → window title sync via Tauri v2 `on_document_title_changed` (KVO on macOS, `DocumentTitleChanged` on Windows); SPA route changes that mutate `document.title` propagate automatically.
- Structured logging via `tracing` + local-time `YYYY-MM-DD HH:MM:SS` timestamps; controlled by `TAURI_HOOK_LOG` (EnvFilter syntax).
- `reqwest` + `rustls-tls` HTTP client (no system OpenSSL dependency); strips `If-None-Match` / `If-Modified-Since` / `If-Match` / `If-Unmodified-Since` / `If-Range` so MISS always pulls full body.

### Build

- `bun run start` — local quick run (`cargo run --release`, incremental).
- `bun run build` — release executable only, no installer.
- `bun run tauri build` — full installer build (`.dmg` / `.msi` / `.nsis`).
- GitHub Actions cross-platform CI: macOS Universal (signed + notarized `.dmg`) and Windows portable (`.exe` + sample `.zip`).

### Known Issues

- **Windows binaries are unsigned** in v1.0.0; Microsoft Defender SmartScreen will display a "Windows protected your PC" warning on first launch. Click **More info** → **Run anyway** to proceed.
- **Cache HIT does not replay upstream security headers**: only `content_type` is replayed from the sidecar. `Set-Cookie`, `Cache-Control`, `X-Frame-Options`, `Content-Security-Policy`, `Strict-Transport-Security`, `Vary`, etc. are not re-emitted. If a cached resource depends on these for correctness or security, clear the entry to force a fresh upstream fetch.
- **macOS DevTools opens an external Safari Web Inspector window** (WKWebView limitation; not a separate panel docked into the app window).
- **Linux is not supported** (WebKitGTK requires private APIs to intercept request-level traffic; out of scope).
- **WebSockets are not intercepted**, **non-GET requests are not cached**, **HTTP conditional requests are stripped**, and **large files (>100 MB) are buffered fully in memory** — see README §8 for the full list.

[1.0.0]: https://github.com/leaker/pouch/releases/tag/v1.0.0
