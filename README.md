[![Build](https://github.com/leaker/pouch/actions/workflows/build.yml/badge.svg?branch=main)](https://github.com/leaker/pouch/actions/workflows/build.yml?query=branch%3Amain)
[![License](https://img.shields.io/github/license/leaker/pouch)](LICENSE)

# Pouch

> Tuck any web app into a local-first desktop pouch.

Pouch is a Rust + Tauri v2 desktop app (macOS + Windows) that wraps any web URL into a native window and transparently caches its HTTP/HTTPS traffic at the platform network layer (NSURLProtocol on macOS, WebView2 `WebResourceRequested` on Windows). Cookies, origin, CSP, and SRI all behave exactly as on the live site, with zero frontend rewriting. A URL-scoped, Tampermonkey-style `inject/*.js` channel lets you run custom JS at `document_start` on a per-pattern basis — useful for offline-first wrappers, CDN-asset overrides, or one-off page patches without touching the upstream site.

The metaphor: like a hamster stuffing food into its cheek pouch, Pouch quietly stashes web resources into a local on-disk cache (`overrides/<host>/<path>`). First load fills the pouch; subsequent loads pull straight from disk with no network request.

## 1. Project introduction

Pouch targets two related needs: "offline-first" desktop wrappers and "local resource override" patches (for example, swapping in a patched copy of a JS file served from a CDN). It uses **platform-native network-stack interception** — every subresource request the webview engine emits while parsing HTML first goes through Rust, which then decides whether to serve a local file or hit the upstream.

The whole flow is **completely transparent** to the frontend: the user lists their target URL(s) in `hook.config.json -> startup_urls`, and on launch the app builds the main webview programmatically with `WebviewUrl::External(<first entry>)` — **no intermediate trampoline page, no IPC bridge**. The origin the webview sees on every request and response is the real `https://www.leelib.com`, so cookies, CSP, SRI, and same-origin policy all behave exactly as on the live site, with zero frontend rewriting. When you do need to inject business scripts into the target page, a separate Tampermonkey-style `inject/*.js` channel is provided — see §6.

How Pouch compares to a CDP-based interception approach (e.g. an Electron app driving `Fetch.requestPaused`):

| Aspect | CDP-based approach | Pouch |
|---|---|---|
| Stack | Electron + Node.js + Chromium | Rust + Tauri v2 + system WebView |
| Interception layer | Chrome DevTools Protocol (CDP) `Fetch.requestPaused` | Platform-native network stack: macOS `NSURLProtocol` / Windows `WebView2 WebResourceRequested` |
| Cookie / Origin / CSP | Automatically correct (CDP intercepts inside the engine) | Automatically correct (native network stack intercepts inside the engine) |
| Platform support | mac / Windows / Linux | mac + Windows (**Linux not supported**, see §7) |
| `Content-Type` recovery | Often guessed from filename on subsequent reads | Sidecar `.meta.json` persists the original response headers |
| Write-to-disk atomicity | Direct `fs.writeFile` | `tempfile::NamedTempFile + persist` (POSIX `rename(2)`) |
| Binary size | ~100 MB+ (bundles Chromium) | ~10 MB (uses the system WebView; **not yet measured, TODO**) |

In short: Pouch delivers equivalent capability with a smaller binary plus the system WebView, structures the cache metadata, and makes disk writes atomic. The trade-offs are: **no Linux support**, and **macOS depends on a private WebKit selector** (same risk profile as Electron — see §9).

## 2. Quick start

### 2.1 Prerequisites

- **Rust toolchain** (rustc 1.77.2+): see [rust-lang.org/install](https://www.rust-lang.org/tools/install)
- **Bun** (recommended) or Node.js + npm/pnpm: see [bun.sh](https://bun.sh)
- **Tauri platform dependencies** (macOS needs Xcode CLT; Windows needs the WebView2 Runtime + MSVC toolchain): full list at [v2.tauri.app/start/prerequisites](https://v2.tauri.app/start/prerequisites/)
- **macOS 11+** (Apple Silicon and Intel both work) or **Windows 10 1809+ / Windows 11**
- **Linux is not supported** — see §7 and §8

### 2.2 Configure the target URL

Edit `hook.config.json` — its location depends on whether you are running a dev tree or a packaged release build (full resolution chain in §4.2):

| Build | Path |
|---|---|
| **dev tree** (`bun run tauri dev`, running from a clone) | `<repo>/hook.config.json` |
| **macOS release** (`Pouch.app` from the .dmg) | `~/Library/Application Support/Pouch/hook.config.json` |
| **Windows release** (portable `.exe` / `.zip`) | next to `pouch.exe` |

```json
{
  "startup_urls": ["https://www.leelib.com"]
}
```

`startup_urls` is an array — the first entry becomes the main window and any additional entries open as extra windows on launch (see §2.6). Setting a single-element array is the **default and recommended** way to configure Pouch. If `startup_urls` is missing, `null`, `[]`, or every entry was non-http(s), Pouch prompts the user with a native NSAlert at startup so the launch can still proceed; **Cancel exits the process** (Escape is equivalent). See §4.1 for the full schema.

> **macOS first launch**: when you double-click `Pouch.app` for the first time, the directory `~/Library/Application Support/Pouch/` does not yet exist. Pouch detects this and seeds it with a copy of the sample `hook.config.json` and the explicit sample `inject/*.js` files shipped inside the .app bundle (`Contents/Resources/sample/`). The bundled sample list is enumerated explicitly in `src-tauri/tauri.conf.json` `bundle.resources` rather than mapped from the whole `inject/` directory, so any debug / scratch `.js` files you keep in the dev-tree `inject/` will **not** sneak into the .app — adding a new sample requires updating that map. Edit the seeded files freely afterwards — Pouch only seeds the directory on first launch and never overwrites your edits. To reset, delete the directory and relaunch.

### 2.3 Install and launch

```bash
git clone <repo>
cd pouch
bun install        # or npm install / pnpm install

# dev mode (hot reload, stdout logs)
bun run tauri dev

# release build — executable only (no installer); fastest path, runs the same on macOS and Windows
bun run build
# artifact: src-tauri/target/release/pouch (Windows: pouch.exe)

# launch the release build; cargo's incremental compilation skips rebuild when sources are unchanged.
# cwd stays at the repo root so inject/ and overrides/ resolve correctly. Same command on macOS and Windows.
bun run start

# full release build with platform installer (.dmg / .msi / .nsis / ...) — rarely needed locally;
# CI (tauri-action on GitHub Actions) produces the macOS installer
bun run tauri build
# artifacts in src-tauri/target/release/bundle/
```

`bun install` only installs a single dev dependency: `@tauri-apps/cli`. Pouch **has no frontend runtime** — the main window is built programmatically in Rust and navigates directly to the first `startup_urls` entry, with no HTML trampoline page, no IPC, and no npm runtime dependencies.

After launch the window opens directly on that URL, and every subresource request flows through the local interception layer. The terminal shows the full startup log:

```
INFO hook: [config] /path/to/hook.config.json startup_urls = 1 entrie(s)
INFO hook: [startup] startup_urls = 1 entrie(s)
INFO hook: [startup] cache root = /path/to/overrides
DEBUG hook: [hook][mac] registerClass -> ok
DEBUG hook: [hook][mac] WKBrowsingContextController.registerSchemeForCustomProtocol: https + http
INFO hook: [hook][mac] NSURLProtocol installed; https/http routed through HookURLProtocol
```

### 2.4 Optional environment variables

```bash
# Tweak log level (to follow the HIT/MISS pipeline)
TAURI_HOOK_LOG=hook=debug bun run tauri dev
```

### 2.5 Open DevTools

The Web Inspector is enabled in both debug and release builds (Pouch is a hook-debugging tool, so devtools always-on is the right default — wired via the `devtools` feature flag on the `tauri` crate).

Press `F12` (or `View → Open DevTools` menu item) to open DevTools. Works on both macOS and Windows.

On macOS the right side of the titlebar carries three SF Symbol buttons mirroring the most-used `View` menu entries: `folder` (Reveal Pouch Folder in Finder, Cmd+Shift+O), `arrow.clockwise` (Reload from Config, Cmd+R), and `wrench.and.screwdriver` (Toggle DevTools, F12). Each button has a hover tooltip showing its keyboard shortcut.

The DevTools button doubles as a state indicator: it shows the outlined `wrench.and.screwdriver` glyph while the inspector is closed and swaps to the filled `wrench.and.screwdriver.fill` glyph while it is open. All three triggers (the titlebar button, F12, and the `View → Open DevTools` menu item) keep the icon in sync — pressing F12 or clicking the menu item flips the icon along with the inspector's visibility.

> **Reload behaviour**: The Reload button (Cmd+R) restarts the application to apply changes to `hook.config.json` and `inject/*.js`. This is implemented as a clean process restart (`app.restart()`) for predictable behavior.

### 2.6 Multiple windows (macOS)

Pouch runs as a single process and supports multiple `WebviewWindow`s sharing the same WebKit data store, so cookies, `localStorage`, and the disk cache are shared across windows. There are two ways to open extra windows:

1. **`startup_urls` array in `hook.config.json`** — the first entry becomes the main window; every subsequent entry is opened as its own additional window when Pouch launches. See §4.1 for the schema.
2. **`File → New Window` (`Cmd+N`)** — pops a native `NSAlert` (titled with a `globe` SF Symbol icon) whose accessory view is a wide `NSComboBox`: type a URL or pick from the dropdown of recently-used destinations. Pressing Return (or clicking *Open*) opens that URL as an additional window. Escape / Cancel / non-http(s) input is a quiet no-op. Recent URLs are persisted across launches in `storage.db` (the `recent_urls` row, see §5.5) next to `hook.config.json`, and the field starts empty so a fresh paste is unobstructed.

Each window gets its own titlebar accessory (Reveal / Reload / Toggle DevTools). The DevTools button is per-window — clicking the button or pressing F12 toggles DevTools on the focused window only — while the Reveal Folder button is process-global and the Reload button still restarts the whole application (so all windows close and reopen with the freshly-read config).

The Cmd+N New Window prompt is currently macOS-only because it is implemented against `NSAlert`; the rest of the multi-window plumbing is cross-platform and the startup `startup_urls` array works on Windows too. The startup-time NSAlert that pops up when `startup_urls` is empty is also macOS-only — on Windows / Linux an empty `startup_urls` resolves with a warn log instead, and Pouch will not open a window until you populate the field.

### 2.7 Menu bar (macOS)

On macOS Pouch installs a HIG-standard menu bar with five top-level menus, replacing Tauri's default macOS menu so the `View` entries (Open DevTools, Reveal Folder, Reload from Config) coexist with the standard editing / window controls:

| Menu | Notable items |
|---|---|
| `Pouch` | About Pouch, Services, Hide / Hide Others / Show All, Quit Pouch (Cmd+Q) |
| `File` | New Window (Cmd+N — see §2.6), Close Window (Cmd+W) |
| `Edit` | Cut / Copy / Paste / Select All (auto-wired to the focused webview) |
| `View` | Open DevTools (F12), Reveal Pouch Folder in Finder (Cmd+Shift+O), Reload from Config (Cmd+R) |
| `Window` | Minimize / Zoom / Close, plus an auto-populated list of open windows with ``Cmd+` `` cycling between them |

The `Window` submenu is tagged with Tauri's `WINDOW_SUBMENU_ID` so AppKit owns the live window list — Pouch never has to reconcile that itself. On Windows only the `View` menu is installed (the OS supplies the rest via the system menu).

### 2.8 Page loading indicator

Every Pouch window opens with the title prefix `⏳ Loading...` set on the underlying `NSWindow` / `HWND` at builder time, so the prefix is visible from the moment the window appears — before WKWebView's first byte arrives. On macOS the titlebar accessory adds a small `NSProgressIndicator` (16pt spinning, indeterminate) at the left edge of the action buttons; it starts on `page_load Started` and stops on `page_load Finished`.

Once the page reports its real `<title>`, Tauri's `on_document_title_changed` swaps the OS window title to it. The `Finished` handler only falls back to a host-derived title if the title still starts with `⏳ Loading...`, so a page that sets `document.title` before navigation finishes is never clobbered.

## 3. How it works

```
┌─────────────────────────────────────────────────────────────┐
│   Main webview (no frontend trampoline page)                │
│                                                              │
│   The Rust side builds the main window programmatically     │
│   with WebviewWindowBuilder, launching directly with        │
│   WebviewUrl::External(url); the webview's first           │
│   frame is the real https://... origin — no intermediate    │
│   page, no location.replace                                  │
│                                                              │
│   Optional initialization_script: the URL-rule dispatcher   │
│   built from inject/*.js (see §6), executed at              │
│   document_start                                             │
│                                                              │
│   document.title changes → on_document_title_changed        │
│   syncs to the OS window title (see §7)                     │
└──────────────────┬──────────────────────────────────────────┘
                   │  native network-stack interception
                   ▼
┌─────────────────────────────────────────────────────────────┐
│              Rust backend (Tauri v2)                        │
│                                                              │
│  hook (native interception entry)                           │
│       │                                                     │
│       │   ┌──── #[cfg(windows)]                             │
│       ├───┤   platform/windows.rs                           │
│       │   │   ICoreWebView2_22 +                            │
│       │   │   WebResourceRequested                          │
│       │   └──── #[cfg(macos)]                               │
│       │        platform/macos.rs                            │
│       │        NSURLProtocol +                              │
│       │        WKBrowsingContextController                  │
│       │        (private selector)                           │
│       ▼                                                     │
│  hook/policy.rs ◄─── platform-shared business logic         │
│       │   - ignore_filter blacklist                         │
│       │   - cache_store::read (HIT serves from local)       │
│       │   - http_fetcher::fetch + write (MISS hits upstream)│
│       │   - 30-second upstream timeout                      │
│       │                                                     │
│       ├── cache_store     atomic write + sidecar metadata   │
│       ├── http_fetcher    reqwest + rustls + strip          │
│       │                   conditional headers               │
│       └── ignore_filter   config-driven blacklist           │
└──────────────────┬──────────────────────────────────────────┘
                   │
                   ▼
        ┌──────────────────────┐
        │  overrides/<host>/   │
        │    <path>            │
        │    <path>.meta.json  │
        └──────────────────────┘
```

**Startup sequence** (see [`src-tauri/src/lib.rs`](src-tauri/src/lib.rs)):

1. `config::load()` resolves `startup_urls`
2. `hook::platform::install_global()` — pre-webview platform setup (macOS registers `NSURLProtocol` plus the private selector; Windows is a no-op)
3. `inject::scan_inject_dir()` + `build_dispatcher_js()` — scans `inject/*.js` and assembles the dispatcher (see §6)
4. `WebviewWindowBuilder::new(app, "main", WebviewUrl::External(startup_urls[0]))` builds the main webview programmatically, attaches `on_document_title_changed`, and optionally attaches the dispatcher as `initialization_script`. Subsequent `startup_urls` entries are opened as extra windows via `dialog::open_extra_window`. If the resolved `startup_urls` list is empty, a `dialog::prompt_initial_url` NSAlert collects a single URL from the user (Cancel exits)
5. `hook::platform::install_for_webview(app.handle())` — post-webview platform setup (Windows attaches `WebResourceRequested` to the main window; macOS is a no-op)

**Request flow** (unified logic; the platform module calls into policy from inside its interception callback):

1. The platform layer receives the intercepted request (method, url, headers)
2. Non-GET / local hosts (`localhost` / `127.0.0.1` / `::1` / `*.localhost`) → `SetResponse` is not called and the webview falls back to its default network stack
3. `ignore_filter::is_ignored(url)` matches → `policy::evaluate` still runs but only fetches without writing (see §4.3)
4. `cache_store::read(cache_key)` hits → assemble a local response (200 + Content-Type from the sidecar)
5. Cache miss → `http_fetcher::fetch` (30-second timeout, conditional headers stripped) → `cache_store::write` (atomic) → response is written back to the webview

The platform layer is responsible only for "how to capture the request / how to send the response"; all business policy lives in `policy.rs` to avoid drift between the two backends.

### 3.1 macOS: NSURLProtocol + WKBrowsingContextController (private selector)

Source: [`src-tauri/src/hook/platform/macos.rs`](src-tauri/src/hook/platform/macos.rs)

Three things happen at startup:

1. `objc2::define_class!` declares `HookURLProtocol : NSURLProtocol` at compile time, with `+canInitWithRequest:` / `+canonicalRequestForRequest:` / `-startLoading` / `-stopLoading` attached
2. `[NSURLProtocol registerClass: HookURLProtocol]`
3. `[WKBrowsingContextController registerSchemeForCustomProtocol: @"https"]` + `@"http"`

The `registerSchemeForCustomProtocol:` selector used in step 3 is a **private selector** on `WKBrowsingContextController` — this is the only public-callable-but-private way to add `https`/`http` to WKWebView's interceptable scheme allow-list. Electron uses the same mechanism internally; the community project [yue/yue (Cheng Zhao's new framework), `nu_custom_protocol.mm`](https://github.com/yue/yue/blob/master/nativeui/mac/browser/nu_custom_protocol.mm) has demonstrated it remains usable through macOS 15. The macOS implementation here is a Rust + objc2 translation of that blueprint.

Request flow: the webview emits an https request → `+canInitWithRequest:` accepts it (GET, non-local host, no `X-Hook-Bypass: 1` marker) → `-startLoading` is invoked on the main thread → snapshot the URL/headers, then `tokio::spawn` onto a dedicated multi-thread runtime to run `policy::evaluate` → on completion, `DispatchQueue::main().exec_async(...)` jumps back to the main thread and sends `didReceiveResponse:` + `didLoadData:` + `URLProtocolDidFinishLoading:` through the `URLProtocolClient`.

### 3.2 Windows: ICoreWebView2_22 WebResourceRequested

Source: [`src-tauri/src/hook/platform/windows.rs`](src-tauri/src/hook/platform/windows.rs)

At startup, `with_webview` is used to obtain `ICoreWebView2Controller`, which is then cast to `ICoreWebView2_22`, calling `AddWebResourceRequestedFilterWithRequestSourceKinds("https://*"/"http://*", ALL_CONTEXTS, ALL_SOURCES)` and registering a callback via `add_WebResourceRequested`.

`ICoreWebView2_22` is a **fully public API** introduced in **Microsoft Edge WebView2 Runtime ≥ 1.0.2210.55** that intercepts every document / iframe / subresource / shared worker / service worker request.

When the cast fails on an older Runtime, the fallback path uses `ICoreWebView2.AddWebResourceRequestedFilter`, which only covers document/iframe loads. A clear warning is logged at startup:

```
WARN hook: [hook][win] ICoreWebView2_22 unavailable; falling back to legacy filter (only document/iframe loads will be intercepted). Update WebView2 Runtime to >= 1.0.2210.55 for full coverage.
```

Expected log output on the happy path (**the Windows code is fully implemented but has only been validated end-to-end on macOS in this repository**):

```
INFO hook: [hook][win] using ICoreWebView2_22 filter (covers iframes/workers)
INFO hook: [hook][win] WebView2 WebResourceRequested handler installed (https/http filtered)
```

The request flow mirrors macOS: the handler runs on the UI thread → snapshot URL/method/headers + `args.GetDeferral()` → jump to the dedicated tokio runtime to run `policy::evaluate` → on completion, `AppHandle::run_on_main_thread` returns to the UI thread, then `environment.CreateWebResourceResponse(stream, 200, "OK", headers)` + `args.SetResponse(response)` + `deferral.Complete()`.

### 3.3 Window title sync

Changes to the webview's `document.title` (including SPA route changes where the router sets `document.title` itself) are automatically synced to the OS window title via Tauri v2's [`WebviewWindowBuilder::on_document_title_changed`](https://docs.rs/tauri/2.9.5/tauri/webview/struct.WebviewWindowBuilder.html#method.on_document_title_changed). It bridges to WKWebView's title KVO on macOS and WebView2's `DocumentTitleChanged` event on Windows, with no frontend JS or IPC required. Pouch **deliberately does not call** `.title(...)` so that the upstream `<title>` takes effect directly.

## 4. Configuration

### 4.1 `hook.config.json`

Resolved location depends on the build (see §2.2 for the table):

- **dev**: `<repo>/hook.config.json`
- **macOS release**: `~/Library/Application Support/Pouch/hook.config.json` (seeded on first launch from the .app bundle's `Contents/Resources/sample/`)
- **Windows release**: next to `pouch.exe`

The shipped [`hook.config.json`](hook.config.json) sets `startup_urls` plus a starter `ignore_urls` list demonstrating all four entry shapes (see §4.3 for the full schema):

```json
{
  "startup_urls": [
    "https://www.leelib.com"
  ],
  "window_dimensions": "inherit",
  "ignore_urls": [
    { "suffix": "gstatic.com",          "comment": "Google static asset CDN (apex + all subdomains)" },
    { "suffix": "googletagmanager.com", "comment": "GTM / GA injection scripts" },
    { "suffix": "google-analytics.com", "comment": "GA reporting endpoint" },
    { "suffix": "cdn.jsdelivr.net",     "comment": "Public npm CDN" },
    { "wildcard": "*.google.com",       "comment": "Example: matches google.com subdomains only (e.g. fonts.google.com / mail.google.com), apex excluded" },
    { "url_wildcard": "https://ipecho.io/*", "comment": "Example: URL glob, * matches any characters" },
    { "url_regex": "^https://example\\.com/track/.*", "comment": "Example: full-URL regex, caller controls anchors" }
  ]
}
```

Fields:

| Field | Type | Required | Description |
|---|---|---|---|
| `startup_urls` | array of strings (each `http://` or `https://`) | No (missing/`null`/`[]` = prompt the user via NSAlert at startup; Cancel exits — see §2.6 / §4.2) | URLs to open at launch. The first valid entry becomes the main `WebviewWindow` (`label = "main"`, `WebviewUrl::External(url)`); every subsequent entry opens as its own extra window with shared cookies / cache and an independent webview lifecycle. Non-http(s) entries are dropped with a `warn` log. See §2.6. |
| `window_dimensions` | `"default"` \| `"inherit"` \| `"maximized"` \| `"fullscreen"` \| `{ "width": <px>, "height": <px> }` | No (default `"inherit"`; first-launch fallback to default 1280×960) | Initial window dimensions applied at launch (uniformly to the main window and every extra window). VSCode-style naming. `"inherit"` restores from `storage.db` — see §5.5. See §4.4. |
| `ignore_urls` | array of entries (one of `suffix` / `wildcard` / `url_wildcard` / `url_regex`, plus optional `comment`) | No (missing/`null`/`[]` = no filtering) | Per-entry blacklist; matched URLs are fetched but never cached. See §4.3 for the four entry shapes. |

> **Schema break in v1.1**: the previous fields `target_url` (single string) and `windows` (array) have been **unified into the single `startup_urls` array**. There is no deprecation alias — users upgrading from v1.0.x must edit `hook.config.json` by hand. The `TAURI_HOOK_TARGET_URL` environment variable and the `argv[1]` URL override have also been removed in the same change (everything goes through `startup_urls` now).

### 4.2 Resolution

Source: [`src-tauri/src/config.rs`](src-tauri/src/config.rs)

`startup_urls` is read from `hook.config.json` only — there is no CLI / env override channel. Resolution **never panics**:

1. **`hook.config.json`** is read from: in dev mode, `<CARGO_MANIFEST_DIR>/../hook.config.json` (the repo root); in macOS release mode, `~/Library/Application Support/Pouch/hook.config.json` (seeded from `Pouch.app/Contents/Resources/sample/` on first launch); in Windows release mode, next to `pouch.exe`; `<cwd>/hook.config.json` is consulted as a last-ditch fallback in every mode. The first candidate that parses wins.
2. Each entry is filtered to http(s); non-http(s) entries are dropped with a `warn` log.
3. **Empty resolved list** (file missing, field omitted, `null`, `[]`, or every entry was non-http(s)) → on macOS, `dialog::prompt_initial_url` pops a native `NSAlert` titled "Welcome to Pouch" asking for a URL; OK opens that URL as the main window, Cancel / Escape calls `std::process::exit(0)`. On Windows / Linux there is no prompt UI, so an empty list is logged at WARN and Pouch refuses to open a window — populate the file and relaunch.

A successful parse prints an INFO log, e.g.:

```
INFO hook: [config] /path/to/hook.config.json startup_urls = 1 entrie(s)
```

> Design note: an earlier internal design called for "panic on missing config", but this was relaxed to "log + fall through" so that Pouch runs out of the box after a clone. The startup-time NSAlert prompt closes the loop on macOS — the user can rescue a missing / empty config without re-opening it manually first.

### 4.3 `ignore_urls` blacklist

Source: [`src-tauri/src/hook/ignore_filter.rs`](src-tauri/src/hook/ignore_filter.rs)

Rules are loaded entirely from `hook.config.json`'s optional `ignore_urls` array — the Rust source ships **no built-in defaults**. Missing field, `null`, or `[]` all mean "no filtering". A match means "**fetch but do not write**": the upstream response is still retrieved and handed to the webview, but the local cache is neither read nor written. The intent is to let implicit browser-emitted telemetry / font requests pass through without polluting `overrides/`.

```json
{
  "ignore_urls": [
    { "suffix": "gstatic.com",          "comment": "Google static asset CDN (apex + all subdomains)" },
    { "suffix": "googletagmanager.com", "comment": "GTM / GA injection scripts" },
    { "suffix": "google-analytics.com", "comment": "GA reporting endpoint" },
    { "suffix": "cdn.jsdelivr.net",     "comment": "Public npm CDN" },
    { "wildcard": "*.google.com",       "comment": "google.com subdomains only (apex excluded)" },
    { "url_wildcard": "https://example.com/api/*", "comment": "URL glob; * matches any characters" },
    { "url_regex": "^https://example\\.com/track/.*", "comment": "full-URL regex; caller controls anchors" }
  ]
}
```

Each entry has **exactly one** of the following four keys — the field name *is* the variant tag (no magic prefix strings on the value). An optional `comment` is documentation-only and ignored at runtime.

| Field          | Match domain | Semantics                                                                                                                                |
|----------------|--------------|------------------------------------------------------------------------------------------------------------------------------------------|
| `suffix`       | host         | Host suffix, **apex included**. `gstatic.com` matches both `gstatic.com` and `fonts.gstatic.com` (but not `notgstatic.com`).             |
| `wildcard`     | host         | Host glob; `*` matches a single label segment and **does not cross `.`**. `*.google.com` matches `fonts.google.com` but **not** `google.com` itself — write a separate `suffix` entry if you need the apex. `ads.*.com` matches `ads.foo.com` but not `ads.foo.bar.com`. |
| `url_wildcard` | full URL     | URL glob; `*` matches **any characters (including `/`)**. All other regex meta is escaped. Auto-anchored at both ends.                   |
| `url_regex`    | full URL     | Raw `regex::Regex` against the full URL. **Not** auto-anchored — the caller controls `^` / `$`.                                          |

Host comparisons parse the URL via the `url` crate, so scheme / port / path / IPv6 brackets are handled correctly. Individual entries that fail to compile (invalid regex, empty / blank value) are warned and skipped without aborting the rest of the list. An entry that omits all four keys, supplies more than one, or uses an unknown key fails JSON parsing for the whole `ignore_urls` array — the loader then warns and falls through with no rules installed.

> **NSURLProtocol constraint behind "fetch but no write"**: see the module docs in [`policy.rs`](src-tauri/src/hook/policy.rs). Once macOS `-startLoading` has been called, the subclass **must** produce a response — there is no NSURLProtocol API to "let go mid-load and fall back to the default loader" — so even on an ignore-list hit we still fetch the body via reqwest and hand it back. The Windows path follows the same semantics so the policy layer can be shared.

### 4.4 `window_dimensions`

Source: [`src-tauri/src/config.rs`](src-tauri/src/config.rs) (schema), [`src-tauri/src/lib.rs`](src-tauri/src/lib.rs) (apply)

Controls the initial window dimensions. VSCode-style naming for clarity — the string variants mirror VSCode's `window.newWindowDimensions` semantics. Four shapes are accepted:

| Value | Behaviour |
|---|---|
| `"default"` | Ordinary floating window at the `DEFAULT_WINDOW_{WIDTH,HEIGHT}` (1280x960) baseline; not maximised, not fullscreen. Useful when the user wants to position / resize the window manually. |
| `"inherit"` (**default**) | Restore the window's position, size, and mode (maximised / fullscreen) from the previous session. State is persisted in `storage.db` (see §5.5) — every Resize / Move gesture writes the new geometry after a 1-second debounce. Falls back to `"default"`'s 1280x960 geometry on first launch (when no state has been recorded yet — i.e. first-launch fallback to default 1280×960). |
| `"maximized"` | Fills the work area — excludes the macOS menubar / dock and the Windows taskbar. Implemented via Tauri's `WebviewWindowBuilder::maximized(true)`, which the underlying wry layer forwards to `NSWindow.zoom:` on macOS and `ShowWindow(SW_MAXIMIZE)` on Windows so the OS work area is honoured natively. |
| `"fullscreen"` | Real fullscreen via `WebviewWindowBuilder::fullscreen(true)` — hides window chrome (title bar, traffic lights, taskbar). |
| `{ "width": <px>, "height": <px> }` | Fixed logical-pixel inner size via `WebviewWindowBuilder::inner_size(width, height)`. Logical pixels are DPR-independent (a `1280` here is the same physical width on a Retina display as on a non-Retina one). |

Examples:

```json
"window_dimensions": "default"
```

```json
"window_dimensions": "inherit"
```

```json
"window_dimensions": "maximized"
```

```json
"window_dimensions": "fullscreen"
```

```json
"window_dimensions": { "width": 1280, "height": 800 }
```

Validation:

- Unknown string values (e.g. `"Maximized"`, `"FULLSCREEN"`, `"max"`, the legacy v1.0.x value `"screen"`) fail JSON parsing for the whole config file (serde `rename_all = "lowercase"`); the loader then warns and falls through, so the resolved `window_dimensions` defaults to `"inherit"` (first-launch fallback to default 1280×960).
- Negative `width` / `height` fail at the `u32` deserialisation step (same fall-through behaviour).
- Zero `width` or `height` is accepted by `u32` but logged as a `warn` at startup and falls back to `"maximized"` mode.

> **Schema break from v1.0.x**: the previous `window` field has been renamed to `window_dimensions` and the value `"screen"` has been renamed to `"maximized"`. There is no deprecation alias — users upgrading from v1.0.x must edit `hook.config.json` by hand.

### 4.5 Logging

The `TAURI_HOOK_LOG` environment variable uses [tracing-subscriber EnvFilter syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html):

```bash
# Default (HIT/MISS plus startup info only)
bun run tauri dev

# Debug level (includes detailed MISS URLs, header conversion errors, etc.)
TAURI_HOOK_LOG=hook=debug bun run tauri dev

# Disable all logging
TAURI_HOOK_LOG=off bun run tauri dev
```

Sample log output (validated end-to-end on macOS — the lead ran it twice with `startup_urls = ["https://www.leelib.com"]`):

```
# First launch (overrides/ empty):
DEBUG hook: MISS key=www.leelib.com/index.html fetching upstream
INFO  hook: MISS key=www.leelib.com/index.html bytes=98203 ct=Some("text/html; charset=utf-8")
INFO  hook: MISS key=www.leelib.com/css/fika.min.xxx.css bytes=30007 ct=Some("text/css")
INFO  hook: MISS key=www.leelib.com/img/logo.webp bytes=1972 ct=Some("image/webp")
... 9 resources MISS

# Second launch (overrides/ kept):
INFO hook: HIT key=www.leelib.com/index.html bytes=98203
INFO hook: HIT key=www.leelib.com/css/fika.min.xxx.css bytes=30007
... all 9 resources HIT, zero network requests
```

## 5. Cache directory layout

Cache root resolves the same way as `hook.config.json` (see §2.2 / §4.2):

- **dev**: `<repo>/overrides/`
- **macOS release**: `~/Library/Application Support/Pouch/overrides/`
- **Windows release**: `overrides/` next to `pouch.exe`

The first call to `cache_store::cache_root()` on the Rust side will `mkdir -p` on demand and emit a `tracing::info!` line to stdout.

```
overrides/
├── .gitkeep
├── www.leelib.com/
│   ├── index.html                    # body
│   ├── index.html.meta.json          # sidecar
│   ├── css/
│   │   ├── fika.min.xxx.css
│   │   └── fika.min.xxx.css.meta.json
│   ├── img/
│   │   ├── logo.webp
│   │   └── logo.webp.meta.json
│   └── images/
│       └── ...
└── api.example.com/
    └── ...
```

### 5.1 Sidecar format (`<file>.meta.json`)

```json
{
  "original_url": "https://www.leelib.com/index.html",
  "content_type": "text/html; charset=utf-8",
  "etag": "\"abcd1234\"",
  "last_modified": "Mon, 01 Jan 2026 00:00:00 GMT",
  "saved_at": "2026-05-07T07:53:24.077843Z"
}
```

| Field | Type | Description |
|---|---|---|
| `original_url` | string | Upstream URL (with scheme + query) — useful for cleanup and debugging |
| `content_type` | string \| null | Upstream `Content-Type` response header; used directly on cache hits, **not guessed from the file extension** (fixes the original Electron-version bug) |
| `etag` | string \| null | Upstream `ETag` response header; not currently used for conditional requests, kept for future use |
| `last_modified` | string \| null | Upstream `Last-Modified` response header; same as above |
| `saved_at` | string | RFC 3339 timestamp of when the entry was written |

### 5.2 Cache key derivation (`cache_key_from_url`)

Source: [`src-tauri/src/cache_store.rs`](src-tauri/src/cache_store.rs)

`https://host/path?query#hash` →

- **No query**: `host/path` (e.g. `https://x.com/foo.js` → `x.com/foo.js`). A path ending with `/` or empty automatically gets `index.html` appended (`https://example.com/` → `example.com/index.html`).
- **With query**: `host/path.__qs_<8 hex chars>__<ext>` — a flat filename suffix, zero extra directory layers. The original filename (extension included) stays as the prefix and the extension is **repeated** at the very end so editors / Quick Look still recognise the file type. Example: `https://x.com/foo.js?v=1` → `x.com/foo.js.__qs_8d2f3e1a__.js`. `find -name 'foo.js*'` lists the no-query entry plus every query variant side-by-side. The hash is `std::hash::DefaultHasher` truncated to 32 bits — a namespacing key, not a security boundary.
- **Fragment** (`#hash`) is intentionally **dropped** — fragments are client-side only and never reach the server, so they do not influence cache identity.
- **Scheme** (`http://` vs `https://`) is intentionally dropped — variants of the same host+path share a cache entry.
- **Percent-encoded path segments** are decoded once before being written to disk (`p%20q.png` → `p q.png` on the filesystem).
- **Query order** is **not** normalised — `?a=1&b=2` and `?b=2&a=1` map to different keys. This matches the conservative behaviour of every common HTTP cache: re-ordering query params can change which resource the upstream serves.

> **Upgrade note**: prior versions of pouch dropped the query from the cache key, so `overrides/` directories created before the query-aware key was added contain entries keyed only by `host/path`. Those entries will never collide with the new `.__qs_<hash>__` filename suffix, but they will also never be reused. If you see stale or wrong content after upgrading, the simplest reset is `rm -rf overrides/* && touch overrides/.gitkeep`.

### 5.3 Atomic writes

`cache_store::write` calls `tempfile::NamedTempFile::new_in(parent)` to write a `.tmp.XXXX` temp file inside the destination directory, calls `sync_all()` to flush, then `persist(<target>)` (which is `rename(2)` underneath, atomic on POSIX). Even with a power loss or `kill -9` mid-write, only the temp file remains — there is **never** a half-written body or a half-written sidecar on disk.

### 5.4 Manual editing / cache invalidation

Edit `overrides/<host>/<path>` directly — the next request will read the new content. To change the `Content-Type`, edit the `content_type` field in the colocated `<path>.meta.json`. To force a miss, delete either the body or the sidecar (`cache_store::read` returns `None` when the sidecar is missing, falling through to the MISS path that re-downloads).

Clear an entire host: `rm -rf overrides/<host>`.
Clear everything: `rm -rf overrides/* && touch overrides/.gitkeep`.

Pouch **does not expose** IPC `clear_*` commands — there is no frontend trampoline page and no IPC bridge. If you need programmatic cache clearing at runtime, re-attach `#[tauri::command]` and a capability to `cache_store`'s `clear_url` / `clear_host` / `clear_all` helpers in your fork, or simply rely on "delete `overrides/` and restart the webview".

### 5.5 `storage.db` (unified state persistence)

Source: [`src-tauri/src/storage.rs`](src-tauri/src/storage.rs)

`storage.db` is a small SQLite database that lives **alongside** `hook.config.json` (same parent directory — i.e. `<repo>/storage.db` in dev, `~/Library/Application Support/Pouch/storage.db` on macOS release, `<exe parent>/storage.db` on Windows / Linux release). It is the single durable home for every cross-session state slot Pouch maintains — currently the most-recent window geometry (so `window_dimensions: "inherit"` can restore where the user left off) and the macOS Cmd+N recent-URLs history (so the dropdown survives relaunches). Future state slots drop into the same table without inventing another file.

Single VSCode-style key/value table:

```sql
CREATE TABLE storage (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL    -- JSON
);
```

| Key | Value JSON shape | Written by |
|---|---|---|
| `window_state` | `{ "x": i32, "y": i32, "width": u32, "height": u32, "maximized": bool, "fullscreen": bool }` | every debounced `Resized` / `Moved` window event (see "Write timing" below) |
| `recent_urls` | `["https://…", "https://…", …]` (most-recent first, capped at 20) | Cmd+N "New Window" prompt on a successful URL submission (macOS only) |

`window_state` field meanings:

| Field | Type | Description |
|---|---|---|
| `x` / `y` | i32 | Outer position in screen pixels (matches Tauri's `outer_position`). |
| `width` / `height` | u32 | Inner size in logical pixels (matches Tauri's `inner_size` — DPR-independent). |
| `maximized` | bool | Whether the window was maximised when last saved. |
| `fullscreen` | bool | Whether the window was in real fullscreen when last saved. |

**Write timing** (`window_state`): every `Resized` / `Moved` window event schedules a write 1 second later. Within the 1-second window, every additional event resets the timer (debounce), so a multi-event drag-resize burst collapses to one row update after the gesture quiesces — the database is **never** updated mid-drag. Updates are single-row UPSERTs (`INSERT … ON CONFLICT(key) DO UPDATE`), so unrelated rows (e.g. `recent_urls`) are never touched by a window-state save.

**Multi-window**: there is one `window_state` row shared across every Pouch window (main + extras from `startup_urls` + Cmd+N). When several windows are alive, the saved state reflects whichever window most recently fired the latest event in the burst. This is intentional — the contract is "open the next session at the last-known geometry", not "track every window separately".

**Recent URLs** (`recent_urls`, macOS only): on a successful Cmd+N URL submission, the entry is prepended to the list, any earlier identical entry is removed (so the dropdown never shows duplicates), and the list is truncated to 20 entries before the row is rewritten. Empty / whitespace-only inputs are ignored. Failed window builds don't write — the dropdown only ever surfaces URLs that successfully opened a window.

**First launch / corrupt database**: missing file, missing row, malformed JSON, or an unwritable user-data dir all resolve to the same fall-back behaviour as the previous JSON implementation — `window_state` becomes the `"default"` mode geometry (1280x960, not maximised, not fullscreen) and `recent_urls` becomes an empty list. The loader silently starts fresh rather than panicking, since state persistence is a UX nicety, never a correctness path.

**Concurrency**: a process-wide mutex serialises every read/write, and SQLite's own crash-recovery (the database file is opened with default journaling) means an `EOPNOTSUPP` mid-write or a hard process kill doesn't corrupt the previously-committed rows.

**Manual editing**: the file is a real SQLite database — open it with `sqlite3 storage.db` (or any SQLite GUI). Inspect with `SELECT key, value FROM storage;`; forget the last-session geometry with `DELETE FROM storage WHERE key='window_state';`; clear recent URLs with `DELETE FROM storage WHERE key='recent_urls';`. Deleting the entire file is also fine — the next launch recreates an empty database and falls back to defaults.

## 6. JS injection

Source: [`src-tauri/src/inject.rs`](src-tauri/src/inject.rs)

Pouch supports a Tampermonkey-style "inject on URL match" mechanism: drop any `*.js` file into the `inject/` directory (resolved the same way as `hook.config.json` — see §2.2: repo root in dev, `~/Library/Application Support/Pouch/inject/` on macOS release, next to `pouch.exe` on Windows release) and at startup the file's frontmatter is parsed and assembled into a dispatcher that is injected into the main webview as `initialization_script`. On every top-level navigation the dispatcher decides which rules to trigger based on `location.href`, and each matching rule executes inside its own function scope.

`inject/` is scanned **recursively**: any `.js` file at any depth is loaded, so you can group scripts by project (`inject/project1/foo.js`, `inject/utils/shared.js`, …) without flattening them into the top level. Symlinks are not followed. Files load in lexicographic order of their full path, so dispatch order is deterministic across platforms and across runs.

Pouch ships two demos ([`inject/global.js`](inject/global.js), [`inject/www.leelib.com/leelib.js`](inject/www.leelib.com/leelib.js)) showing two typical patterns — "console output on every URL" and "banner injection on a specific site" — that you can edit or remove freely.

### 6.1 Frontmatter syntax

```js
// ==UserScript==
// @name leelib custom banner
// @match https://www.leelib.com/*
// @match regex:^https://(api|cdn)\.example\.com/
// ==/UserScript==

(function () {
  // your script body
})();
```

| Field | Required | Description |
|---|---|---|
| `@name` | No | Used only for log readability; falls back to the filename (without `.js`) if absent |
| `@match` | **Yes** | Match rule; multiple lines are allowed and **any one match** triggers the script |

`@match` supports two forms:

- **Glob form** (default): `*` matches any character (including `/`), other characters match literally, both ends are auto-anchored. Examples: `https://*.example.com/*`, `@match *`
- **Regex form**: prefixed with `regex:` and followed by JS regex source without delimiters, e.g. `regex:^https://(api|cdn)\.x\.com/`. Invalid regexes are caught at runtime by the dispatcher's `try/catch` and reported via `console.error`, without affecting other rules

Tampermonkey fields like `@grant` / `@require` / `@run-at` / `@noframes` / `@version` are **not parsed** but are silently allowed, so existing userscripts can be pasted in directly.

A file with no `@match` line is warned about and skipped at startup; it does **not** silently match every URL.

### 6.2 Run timing and isolation

- Injected at **document_start**, before the target page's own JS (same as Tampermonkey `@run-at document-start`)
- **SPA route changes do not re-trigger**: the script runs once per real top-level navigation. Hook `history.pushState` / `popstate` yourself if you need to react to client-side route changes
- Each rule is wrapped by the dispatcher in its own `try/catch`, so one script throwing **does not** affect any others
- The dispatcher uses the `Function` constructor to execute rule code, so each rule has its own function scope. The dispatcher's outer IIFE `'use strict'` does **not** propagate into rule bodies — rules can opt in themselves
- Startup logs report rule count and whether the dispatcher was actually installed:

```
INFO hook: [inject] scanning /path/to/inject
INFO hook: [inject] loaded rule "leelib custom banner" (1 pattern(s)) from .../leelib.js
INFO hook: [startup] inject rules = 2 (dispatcher WILL be attached)
```

### 6.3 How to disable (by granularity)

| Goal | Action |
|---|---|
| No injection at all | Delete the entire `inject/` directory. Logs show `dispatcher will NOT be attached` and there is zero runtime overhead |
| Drop one specific rule | Delete the corresponding `.js` file |
| Disable temporarily, keep the file | Comment out every `@match` line; startup warns and skips this file, others are unaffected |
| Change the trigger conditions | Edit / add / remove `@match` lines |

### 6.4 Known limitations and notes

- **`@match *` matches every URL** — including subframes / iframes inside the target page (`about:blank`, `data:`, etc.). Narrow it to at least `@match https://*` to match only http(s) origins
- **No `@grant` / GM_* APIs**: the dispatcher is a bare JS execution environment and does not emulate Tampermonkey APIs like `GM_setValue` / `GM_xmlhttpRequest`. Use `localStorage` / `IndexedDB` if you need persistence
- **Cross-origin fetch / auth scripts**: scripts run on the target site's origin and are subject to the same-origin policy. They cannot directly read resources from other origins, identical to the behaviour of a browser extension's content script

## 7. Cross-platform notes

### Platform support matrix

| Platform | Status | Notes |
|---|---|---|
| macOS 13 / 14 / 15 | Fully supported (validated) | NSURLProtocol + private selector; the lead has run two cycles of MISS+HIT across 9 resources against `https://www.leelib.com` |
| macOS 11 / 12 | Should work (**not validated**) | The private selector has existed since macOS 10.10; objc2 0.6 and dispatch2 0.3 also support older macOS versions |
| macOS 17+ | Future risk | Apple may remove `WKBrowsingContextController.registerSchemeForCustomProtocol:`; same risk as Electron |
| Windows 11 / 10 1809+ | Implementation complete (**not validated**) | `ICoreWebView2_22` public API; requires WebView2 Runtime ≥ 1.0.2210.55 |
| Windows older Runtime | Fallback | Auto-falls back to `AddWebResourceRequestedFilter`; covers document/iframe only, **not** subresources / workers |
| Window title | Auto-synced | Bridged via Tauri v2 `on_document_title_changed` to WKWebView title KVO on macOS and WebView2 `DocumentTitleChanged` on Windows; SPA route changes that update `document.title` also fire |
| Linux | Not supported | WebKitGTK likewise requires private APIs to intercept request-level network traffic; this project does not invest in it. Use Electron if you need Linux |

### macOS App Store

**Cannot be shipped through the App Store** — Apple's MAS review scans for private selectors and will reject. Pouch is positioned as a developer / personal-use tool, not for store distribution. If you need the store channel, fall back to the Electron + CDP route (Electron likewise depends on a set of Apple private APIs, but its entitlements are tacitly accepted by Apple; third parties travelling the same path face higher risk).

## 8. Known limitations

- **WebSockets are not intercepted**: neither backend can capture WebSocket frames after the upgrade handshake; same limitation as Electron CDP (CDP also only intercepts HTTP)
- **POST / non-GET is not cached**: `policy::evaluate` only handles GET. The platform layer checks the method at the entry point and lets non-GET through directly (the macOS path returns NO from `+canInitWithRequest:`; the Windows path returns from the handler without calling `SetResponse`), letting the webview use its default network stack
- **No HTTP conditional requests**: `http_fetcher::fetch` actively strips `If-None-Match` / `If-Modified-Since` / `If-Match` / `If-Unmodified-Since` / `If-Range`, always pulling the full body. Hits serve from cache without a conditional request; ETag / Last-Modified are recorded in the sidecar for future use only
- **fragment is dropped from the cache key**: `#a` and `#b` share one cache entry (this is correct per HTTP — fragments never reach the server). Distinct query strings, on the other hand, **do** map to distinct cache entries (`?v=1` and `?v=2` are stored separately) so dynamic signed URLs like `?time=…&sign=…` no longer replay stale tokens — see §5.2
- **Top-level navigation also goes through the interceptor**: the main webview is started programmatically with `WebviewUrl::External(startup_urls[0])`, and the first frame's top-level document request is **also** covered by the native interception layer (macOS NSURLProtocol and Windows WebView2 WebResourceRequested both catch it), so `index.html` is cached on first launch
- **JS injection does not re-run on SPA route changes**: `inject/*.js` runs once at document_start; pseudo-navigations performed by frontend frameworks via `history.pushState` will **not** re-trigger the rules. Hook the history API yourself if you need to react to route changes (see §6.2)
- **`@match *` matches every URL**: including `about:blank` and `data:` subframes. Narrow it to at least `@match https://*` to match only http(s) origins
- **macOS / Windows only**: Linux does not work (see §7)
- **Cookie isolation**: cookies are split between two stores. The **webview** owns its own cookie jar (`NSHTTPCookieStorage` on macOS, the WebView2 cookie manager on Windows) and **reqwest** keeps its own in-process jar (enabled via `cookie_store(true)`). On a cache MISS / ignore-list passthrough, upstream `Set-Cookie` headers are forwarded verbatim to the webview (so it stores the cookie and replays it on subsequent requests) **and** stored in reqwest's jar (so further reqwest-driven fetches in the same session also carry it). The two jars are not bidirectionally synchronised, so cookies set by JS inside the webview are not visible to reqwest, and vice versa. `Set-Cookie` is **never** persisted in the cache sidecar — replay would leak a stale session cookie — so cache HITs serve the body without re-emitting cookies, leaving whatever the live cookie jars hold untouched
- **Cache HIT replays upstream headers from the sidecar**: on a cache HIT, every header captured from the original upstream response (CORS `Access-Control-Allow-Origin` / `Access-Control-Allow-Credentials`, `Cache-Control`, `X-Frame-Options`, `Content-Security-Policy`, `Strict-Transport-Security`, `Vary`, etc.) is re-emitted to the webview alongside `content_type`, keeping CORS / framing / caching behaviour identical between the first MISS and subsequent HITs. `Set-Cookie` is the explicit exception (see "Cookie isolation" above). Sidecars written by older pouch builds — before this field existed — fall back to an empty replay list (`#[serde(default)]`); clear the affected `overrides/<host>/` subtree to refresh them
- **Large files are buffered fully in memory**: `cache_store::read/write` loads each entry into a single `Vec<u8>`; resources > 100 MB may OOM (inherited from v1, left as future work to redo with streaming)
- **WebView2 Runtime version requirement**: `ICoreWebView2_22` requires Runtime ≥ 1.0.2210.55 (early 2024); older versions fall back to document/iframe-only interception, with a clear log line prompting the user to upgrade the Runtime

## 9. Comparison with a CDP-based interception approach

For context, here is how Pouch differs in detail from a CDP-driven interception approach (e.g. an Electron app that drives `Fetch.requestPaused` over the Chrome DevTools Protocol):

| Aspect | CDP-based | Pouch |
|---|---|---|
| Interception method | CDP `Fetch.requestPaused` | macOS `NSURLProtocol` + Windows `WebView2 WebResourceRequested` |
| Cookie / Origin / CSP | Automatically correct | Automatically correct (same abstraction layer) |
| Cache key algorithm | `host + pathname` (typical) | `host + pathname[__qs<query-hash>]` (query-aware) |
| Cache directory layout | `overrides/<host>/<path>` (typical) | `overrides/<host>/<path>` |
| `Content-Type` on cache hit | Often lost (raw `fulfillRequest` without headers) | Sidecar stores `content_type`, read directly on hits |
| Write atomicity | Direct `fs.writeFile` (crash leaves a half-written file) | `tempfile::NamedTempFile + persist` (POSIX `rename(2)`) |
| HTTP `Range` requests | Often not supported (raw full-body `fulfillRequest`) | Platform layer always responds with the full body; `Range` is handled by the engine (the standard NSURLProtocol / WebResourceRequested model) |
| Sidecar metadata | None | `original_url` / `content_type` / `etag` / `last_modified` / `saved_at` |
| Frontend transparency | Full (CDP intercepts inside the engine) | Full (native network stack intercepts; no frontend trampoline page either) |
| User-script injection | Roll your own | Tampermonkey-style `inject/*.js`, URL-rule dispatcher injected at document_start (see §6) |
| Window title sync | Engine default (built into Chromium) | Tauri v2 `on_document_title_changed` bridges WKWebView KVO / WebView2 `DocumentTitleChanged` |
| Platform support | mac / win / linux | mac / win |
| Private API dependency | None (CDP is a public Chromium protocol) | macOS only: `WKBrowsingContextController.registerSchemeForCustomProtocol:` |
| Mac App Store | OK (Electron entitlements tacitly accepted) | Not OK (private selector is a hard reject) |
| HTTP client | Node.js default (OpenSSL) | reqwest + rustls-TLS (no system OpenSSL dependency) |
| Logging | `console.log` | `tracing` structured logs, controlled via `TAURI_HOOK_LOG` |
| Binary size | ~100 MB+ (bundles Chromium) | ~10 MB (**not measured** — measure with `tauri build` in your fork and update this row) |

**Key trade-off**: Pouch trades a private-API dependency on macOS (same risk as Electron) and the lack of Linux support for a much smaller binary, structured cache metadata, atomic writes, and a built-in URL-rule injection mechanism. If your goal is the Mac App Store or Linux, the Electron + CDP route remains the right pick.

## 10. Project layout

```
pouch/
├── README.md              # this document
├── LICENSE                # MIT
├── hook.config.json       # user config: startup_urls + window_dimensions + ignore_urls
├── package.json           # only one devDep: @tauri-apps/cli
├── bun.lock
├── inject/                # user-script directory (see §6)
│   ├── global.js                    # demo: @match * console output on every URL
│   └── www.leelib.com/leelib.js     # demo: @match https://www.leelib.com/* banner
├── src-tauri/
│   ├── Cargo.toml         # cfg-gated platform deps (webview2-com / objc2-*)
│   ├── tauri.conf.json    # minimal: app.windows = [] (window built in Rust)
│   ├── capabilities/default.json   # core:default
│   ├── icons/
│   ├── build.rs
│   └── src/
│       ├── main.rs
│       ├── lib.rs            # wiring: config::load + install_global + programmatic webview + install_for_webview
│       ├── inject.rs         # scans inject/*.js + frontmatter parsing + dispatcher generation
│       ├── config.rs         # json file only (CLI / env override removed in v1.1)
│       ├── cache_store.rs    # atomic write + sidecar + clear_* helpers (no longer wired to IPC)
│       ├── http_fetcher.rs   # reqwest + rustls + strip conditional headers
│       ├── dialog.rs         # macOS NSAlert URL prompt + open_extra_window
│       ├── titlebar.rs       # macOS NSTitlebarAccessoryViewController three buttons + spinner
│       ├── storage.rs        # SQLite-backed window_state / recent_urls persistence
│       ├── bootstrap.rs      # macOS first-run copy default config from .app/Contents/Resources/sample/
│       ├── util.rs           # pretty_path / lexical_normalize + macos_app_support_dir + DEFAULT_WINDOW_WIDTH/HEIGHT
│       └── hook/
│           ├── mod.rs
│           ├── ignore_filter.rs    # config-driven ignore_urls matchers
│           ├── policy.rs           # Decision::{Respond, Bypass} shared business logic
│           └── platform/
│               ├── mod.rs          # cfg dispatch; install_global / install_for_webview
│               ├── macos.rs        # NSURLProtocol + private selector (install_global)
│               └── windows.rs      # WebView2 + add_WebResourceRequested (install_for_webview)
└── overrides/             # cache root (created at runtime)
    └── .gitkeep
```

> Local-only directories (gitignored, never tracked): `tasks/` (planning notes + decisions), `tests/` (scratch test dir), `node_modules/`, `target/`, `dist/`, plus `storage.db*` runtime state files. See [`.gitignore`](.gitignore) for the full list.

> Architectural note: there is no `frontend/` directory and no `commands.rs` — Pouch deliberately has zero frontend runtime and zero IPC surface. `tauri.conf.json` does not declare `build`, `app.security`, or `app.withGlobalTauri`; the main window is built programmatically in `lib.rs` with `WebviewWindowBuilder::new(app, "main", WebviewUrl::External(startup_urls[0]))`.

## 11. License

MIT — see the [LICENSE](./LICENSE) file.
