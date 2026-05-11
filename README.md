[![Build](https://github.com/leaker/pouch/actions/workflows/build.yml/badge.svg?branch=main)](https://github.com/leaker/pouch/actions/workflows/build.yml?query=branch%3Amain)
[![License](https://img.shields.io/github/license/leaker/pouch)](LICENSE)

# Pouch

> Desktop browser for tweaking any site — inject scripts, swap resources, persist overrides.

Pouch turns any web page into a standalone desktop app on macOS and Windows, then lets you stack your own changes on top: drop in a userscript and it runs at `document_start`; save a tweaked copy of any served file and Pouch serves it back on the next reload. Cookies, origin, and session behavior match the live site exactly — you're not rewriting the page, you're layering on top of it.

## Features

- Run any web page as a standalone desktop app — one entry in a config file, no wrapper code to write.
- Open multiple pages at once; each gets its own window, all sharing the same cookies and storage.
- Inject your own JavaScript with Tampermonkey-style `@match` rules — runs before the page's own scripts.
- Replace any served file (JS, CSS, image, HTML…) with a local copy, just by editing the file on disk.
- Tweaks survive reloads automatically — no DevTools session to keep open, no rebuild step.
- Skip the cache on chosen hosts or URL patterns (analytics, telemetry, CDNs) so they always hit the network.
- Configure startup URLs, window size, and ignore rules in a single TOML file with inline docs.
- DevTools available with F12. On macOS, three titlebar buttons add reveal-folder / reload-from-config / toggle-devtools shortcuts.

## Install

### macOS — Homebrew (recommended)

```bash
brew install --cask leaker/tap/pouch
```

Updating:

```bash
brew upgrade --cask pouch
```

On first launch Pouch asks to trust a local certificate so it can view and modify HTTPS traffic for the page you load. Approve once at the system prompt and you're done — there is no second prompt on later launches.

### Windows — Scoop (recommended)

```pwsh
scoop bucket add leaker https://github.com/leaker/scoop-bucket
scoop install pouch
```

Updating:

```pwsh
scoop update pouch
```

If SmartScreen flags the binary on first run, right-click the file → **Properties** → **Unblock**, then relaunch. Pouch is open source but not Authenticode-signed yet.

### Direct download

For users not on brew/scoop, grab the latest build from the [releases page](https://github.com/leaker/pouch/releases/latest):

- **macOS** — `Pouch-<version>.dmg` (universal, signed and notarized)
- **Windows** — `Pouch-<version>.exe` (portable binary; double-click to run) or `Pouch-<version>.zip` (portable binary plus sample `hook.conf.toml` and `inject/` folder)

## First run

On launch Pouch reads `hook.conf.toml` and opens every URL listed in `startup_urls` as its own window. The default config opens `https://www.leelib.com` — replace it with the site you actually want.

- **macOS**: if `startup_urls` is empty Pouch pops a native prompt asking for a URL. The first time you run Pouch you'll also get a one-shot system prompt asking to trust a local certificate (so Pouch can view and modify HTTPS traffic).
- **Windows**: if `startup_urls` is empty Pouch logs a warning and waits — populate the file and relaunch.

Press **F12** any time to open DevTools. On macOS, the right side of the title bar carries three shortcut buttons — reveal the Pouch data folder in Finder, reload from config (`Cmd+R`), toggle DevTools (`F12`) — and `Cmd+N` opens a "new window" prompt with a URL combobox that remembers recent destinations.

## Configuration

Pouch reads a single TOML file on launch. Edit it and hit Reload (`Cmd/Ctrl+R`) to apply.

| Platform | Path |
|---|---|
| macOS | `~/Library/Application Support/Pouch/hook.conf.toml` |
| Windows | Next to `pouch.exe` (portable layout) |

A fully-commented default is seeded on first launch and never overwritten — open it in your editor and tweak in place.

### Fields at a glance

- `startup_urls` — URLs to open at launch. The first entry becomes the main window; each additional entry opens as its own extra window with shared cookies and storage.
- `window_dimensions` — `"inherit"` (default; restore last session's geometry), `"default"` (1280×960), `"maximized"`, `"fullscreen"`, or `{ width = 1600, height = 1000 }`.
- `ignore_urls` — URL patterns Pouch passes through without caching. Useful for analytics endpoints, font CDNs, or anything you don't want sitting in `overrides/`. Four match modes:
  - `suffix` — host suffix including the apex (`{ suffix = "gstatic.com" }` matches both `gstatic.com` and `fonts.gstatic.com`).
  - `wildcard` — host glob; `*` matches one label and does not cross `.` (`{ wildcard = "*.google.com" }` matches `fonts.google.com` but not `google.com`).
  - `url_wildcard` — full-URL glob; `*` matches any characters including `/`. Anchored at both ends.
  - `url_regex` — raw regex against the full URL; you control the anchors.

See the [sample `hook.conf.toml`](hook.conf.toml) at the project root for inline documentation on every field, including how parse failures are handled.

## Customize a site

Pouch keeps everything you can tweak inside its data folder:

| Platform | Data folder |
|---|---|
| macOS | `~/Library/Application Support/Pouch/` |
| Windows | Next to `pouch.exe` (portable layout) |

Use the title bar's reveal-folder button (or `View → Reveal Pouch Folder`, `Cmd+Shift+O` on macOS) to jump straight there.

### Inject your own scripts

Put a `.js` file anywhere under `inject/` and declare which URLs it runs on with a Tampermonkey-style header. Pouch scans `inject/` recursively at launch, so feel free to organize by host, by feature, or however you like — what decides the match is the `@match` line, not the folder name.

```js
// ==UserScript==
// @name   tweak leelib
// @match  https://www.leelib.com/*
// ==/UserScript==

(function () {
  console.log('[leelib] running on', location.host);
  // your tweaks here
})();
```

Rules:

- **`@match`** — required. Glob form by default (`*` matches any characters, crossing `/`); prefix with `regex:` to switch to a real regex (`@match regex:^https://(api|cdn)\.example\.com/`). Multiple `@match` lines are allowed; any one matching triggers the rule.
- **`@name`** — optional label that shows up in startup logs.
- **No `@match` line** → file is skipped at scan time with a warning. Comment out every `@match` to disable a script temporarily without deleting it.
- Scripts run at **document_start**, before the page's own JavaScript, in their own try/catch — a thrown error in one script never breaks the others.
- SPA route changes do **not** re-trigger scripts. Hook the History API yourself if you need that.

A safer default than `@match *` is `@match https://*`, which skips `about:blank` and `data:` frames.

### Replace served files (overrides)

The first time Pouch fetches a resource it stashes the response under `overrides/<host>/<path>`, with a small `.meta.json` sidecar holding the original headers. On the next load Pouch serves the file straight from disk — no network request.

```
overrides/
  cdn.example.com/
    static/
      bundle.js
      bundle.js.meta.json
      site.css
      site.css.meta.json
```

URLs with a query string get a hashed suffix in the filename: `?v=1` makes `bundle.js` land as `bundle.js.__qs_<8-char-hash>__.js`. Different queries map to different files so a stale signed URL never replaces the live one. The trailing `.js` (or `.css`, `.png`, ...) is repeated so your editor still picks the right syntax highlighting.

To customize a response, edit the file on disk and reload. To fall back to the live response, delete the file (and its `.meta.json`) — Pouch will refetch and re-cache on the next load.

Hosts and URL patterns listed in `ignore_urls` are fetched but never written to `overrides/`, which is the right setting for analytics endpoints and other things you don't want to freeze.

## Uninstall

### macOS (Homebrew)

```bash
brew uninstall --cask --zap pouch
```

`--zap` clears `~/Library/Application Support/Pouch/` and related caches. The trusted certificate stays in your Keychain — open Keychain Access, search for "Pouch", and delete the entry manually if you want it gone. Drop `--zap` to keep your inject scripts and overrides.

### Windows (Scoop)

```pwsh
scoop uninstall pouch
```

`scoop uninstall pouch` removes the application but keeps your `hook.conf.toml`, `inject/`, and `overrides/` in scoop's persist folder (typically `~/scoop/persist/pouch/`). Use `scoop uninstall pouch --purge` to wipe those as well.

### Manual install

Drag `Pouch.app` to the Trash on macOS, or delete the install directory on Windows. Then remove the data folder listed under [Customize a site](#customize-a-site) if you want to wipe your tweaks too.

## Build from source

Requires the Rust toolchain and the [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/) for your platform.

```bash
git clone https://github.com/leaker/pouch
cd pouch

cargo dev               # development with hot-reload
cargo start             # run the release binary without bundling
cargo build-app         # release binary into src-tauri/target/release/
cargo bundle            # host-arch bundle (macOS .dmg; quick local check)
cargo bundle-universal  # universal macOS .dmg, matches CI's release artifact
```

See `.cargo/config.toml` for the full alias list.

## License

[MIT](LICENSE).
