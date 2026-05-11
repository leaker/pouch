# Releasing Pouch

This document describes how to cut a new Pouch release and how the two
downstream package manager repos (`leaker/homebrew-tap`,
`leaker/scoop-bucket`) get bumped automatically.

It is intended for the maintainer. End users do not need to read this.

---

## TL;DR — releasing a new version

1. Bump the version in `src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml`,
   and `src-tauri/Cargo.lock` to `X.Y.Z`.
2. Commit, then tag:
   ```bash
   git commit -am "chore: bump version to X.Y.Z"
   git tag vX.Y.Z
   ```
3. Push the commit AND the tag:
   ```bash
   git push origin main
   git push origin vX.Y.Z
   ```
4. Wait. From here everything is automated:
   - The tag push triggers `.github/workflows/build.yml`, which builds the
     macOS `.dmg` (signed + notarized) and Windows `.exe` / `.zip`, then
     publishes a GitHub Release with all three assets attached.
   - The moment the Release is published, GitHub fires a
     `release.published` event that triggers
     `.github/workflows/bump-package-managers.yml`. That workflow downloads
     `Pouch-X.Y.Z.dmg` + `Pouch-X.Y.Z.zip`, computes their sha256, and
     pushes a `pouch: bump to X.Y.Z` commit to `leaker/homebrew-tap` and
     `leaker/scoop-bucket`.
5. End users running `brew upgrade --cask pouch` or `scoop update pouch`
   now resolve to the new version within seconds.

The whole flow is hands-off after step 3. If anything goes wrong at the
bump stage you can re-run it manually — see [Manual / backfill bump](#manual--backfill-bump)
below.

---

## One-time setup: `PACKAGE_MANAGER_BUMP_TOKEN`

`bump-package-managers.yml` needs to push commits into the homebrew-tap
and scoop-bucket repos. The default `GITHUB_TOKEN` is scoped only to the
workflow's own repo (`leaker/pouch`), so cross-repo pushes require a
Personal Access Token configured as a repository secret.

### Option A — classic PAT (simpler)

1. Visit <https://github.com/settings/tokens> → **Generate new token
   (classic)**.
2. Name: `pouch package manager bump`
3. Expiration: pick what suits you. Recommended:
   - `No expiration` if you trust your machine and want zero maintenance.
   - `1 year` with a calendar reminder to rotate 30 days before expiry.
4. Scopes: tick **`repo`** (full control of private repositories — this
   covers public ones too).
5. Click **Generate token**, copy the value once.
6. In `leaker/pouch` on GitHub: **Settings → Secrets and variables →
   Actions → New repository secret**.
   - Name: `PACKAGE_MANAGER_BUMP_TOKEN`
   - Value: paste the PAT
7. Save.

### Option B — fine-grained PAT (least privilege, recommended)

1. Visit <https://github.com/settings/personal-access-tokens/new>.
2. Name: `pouch package manager bump`
3. Resource owner: `leaker`
4. Repository access: **Only select repositories** → tick
   `leaker/homebrew-tap` and `leaker/scoop-bucket`.
5. Repository permissions: set **Contents** to **Read and write**.
   Leave everything else as **No access**.
6. Generate, copy the token once.
7. Add as `PACKAGE_MANAGER_BUMP_TOKEN` in `leaker/pouch` repo secrets
   (same path as Option A step 6).

Fine-grained tokens currently max out at 1 year — put a calendar
reminder for 30 days before expiry.

### Verifying setup

Once the secret is configured, you can dry-run the bump workflow against
the latest existing release without cutting a new tag:

- Go to `leaker/pouch` → **Actions** → **Bump package manager
  manifests** → **Run workflow**.
- Leave `tag` blank to target the latest published release, or type an
  explicit tag like `v2.0.1`.

A successful run will produce a `pouch: bump to X.Y.Z` commit on the
`main` branch of each downstream repo (or print "No diff — skipping
commit" if the manifests are already up to date).

---

## Manual / backfill bump

Use the `workflow_dispatch` trigger when you need to:

- Re-run the bump after rotating the PAT (the original run failed
  because the token had expired).
- Re-run the bump after fixing a hand-edited manifest in a downstream
  repo.
- Initially seed the downstream repos against a release that predates
  this workflow.

**Procedure**: `leaker/pouch` repo → **Actions** → **Bump package
manager manifests** → **Run workflow** → enter the tag (`vX.Y.Z`) or
leave blank for the latest published release → **Run workflow**.

The workflow is idempotent: re-running it against a tag that's already
bumped is a no-op (it will print "No diff — skipping commit" and exit
cleanly), so it is safe to retry.

---

## Pre-releases (rc / beta tags)

Tags like `v2.0.0-rc1` go through the same build pipeline and produce
the same `Pouch-2.0.0-rc1.dmg` / `Pouch-2.0.0-rc1.zip` assets. However,
the bump workflow **deliberately skips** any release that is marked as
`prerelease` or `draft` on GitHub — we don't want an rc build to
displace the stable version in Homebrew / Scoop.

If you want to publish a pre-release through the package managers
anyway, you'd need a separate Cask / bucket entry for the beta channel.
That's intentionally not wired up today.

---

## Downstream repos

| Repo                       | What lives there        | What the bump workflow rewrites                                                  |
|----------------------------|-------------------------|----------------------------------------------------------------------------------|
| `leaker/homebrew-tap`      | `Casks/pouch.rb`        | `version "X.Y.Z"`, `sha256 "..."` (the .dmg checksum)                            |
| `leaker/scoop-bucket`      | `bucket/pouch.json`     | `version`, `architecture.64bit.url`, `architecture.64bit.hash` (the .zip sha256) |

End-user install instructions (Homebrew tap, Scoop bucket URLs, etc.)
live in the project README, not here.

---

## What the workflows actually do

- **`.github/workflows/build.yml`** — triggers on `push: tags: ['v*']`
  and on `workflow_dispatch`. Builds the macOS `.dmg` (universal, signed
  + notarized) and the Windows portable `.exe` + `.zip`, then publishes
  a GitHub Release with all three attached. See the comments at the top
  of that file for the full secret list (`APPLE_*`, `KEYCHAIN_PASSWORD`).
- **`.github/workflows/bump-package-managers.yml`** — triggers on
  `release: types: [published]` (after `build.yml` has finished
  uploading) and on `workflow_dispatch`. Downloads the just-published
  assets, computes sha256, rewrites both downstream manifests, commits +
  pushes. Needs `PACKAGE_MANAGER_BUMP_TOKEN` (this file).
