# AgentKeeper

**English** | [简体中文](README.zh-CN.md)

> One local control plane for AI coding-agent assets — discover, validate, edit,
> distribute, audit and roll back **skills, MCP servers, plugins, rules and memory**
> across global and project scopes.

[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![platform: Windows](https://img.shields.io/badge/platform-Windows-0078D4.svg)](#requirements)
[![shell: Tauri 2](https://img.shields.io/badge/shell-Tauri%202-24C8DB.svg)](https://tauri.app/)
[![core: Rust](https://img.shields.io/badge/core-Rust-B7410E.svg)](tauri-shell/src-tauri)

5 asset kinds × 17 agents × 2 scopes, one window. The desktop shell is **Tauri 2**, the
business logic is **Rust** (`tauri-shell/core/src/`), and the UI is a zero-dependency
native DOM app (`src/renderer/`) with no build step.

---

## Why

An agent's behaviour comes from the files it silently preloads, not only from the model:

| Asset | Carrier | How it fails |
|---|---|---|
| Skill | `SKILL.md` + YAML frontmatter | missing `description` → never triggers; folder ≠ `name` → wrong cache key |
| MCP server | `mcp.json` / `.mcp.json` / `config.toml` | missing `command`/`url` → connect fails; a rewrite swallows sibling servers |
| Plugin | `.codebuddy-plugin/plugin.json` etc. | version directory drift; cache mistaken for a marketplace |
| Rule | `AGENTS.md` / `CLAUDE.md` / `.mdc` | empty file still burns tokens; loaded twice by two agents |
| Memory | `MEMORY.md` + `memory/YYYY-MM-DD.md` | stale, conflicting, unaudited |

Those files are scattered across `~/.claude`, `~/.codex`, `~/.cursor`, `~/.workbuddy` and the hidden
directories of every repository. AgentKeeper makes them visible, editable, verifiable and reversible
from one place.

## Features

- **Discover** — scans 17 agents plus the shared `~/.agents/skills` directory; global and project scope.
- **Show effective state** — which copy actually wins, and what a project scope shadows.
- **Validate** — frontmatter, JSON/JSONC, TOML and YAML checks; security scan for risky MCP entries.
- **Edit safely** — every write goes straight into the real agent directory as a transaction: the
  current state of the paths it touches is captured in memory first, then written atomically and
  journalled. A write that fails part-way is rolled back to that capture, and a write that cannot
  be captured completely is refused before anything moves.
- **Confirm before writing** — every create, update and delete asks for confirmation first, and the
  dialog lists the real paths about to be written. Settings are the exception: they write to the
  app's own store, not to an agent directory.
- **Distribute** — copy a skill/rule/memory/MCP entry to other agents or registered projects.
- **Audit** — append-only journal plus baseline fingerprints that report silent edits. The journal
  is a record of what changed, not an undo menu: there is no per-transaction rollback.
- **Work headless** — the same core ships as a 15-tool stdio MCP server (`agentkeeper-mcp`), so an
  agent can manage its own assets through the same validate → capture → write → journal path.

## Requirements

| Dependency | Notes |
|---|---|
| Rust stable (`x86_64-pc-windows-msvc`) | builds `tauri-shell/src-tauri`; needs MSVC build tools + Windows SDK |
| WebView2 Runtime | included with Windows 11 |
| Node.js | only for the `fs`/`path` helper scripts (frontend sync, lint, i18n gate). Running the app does not need Node |

`rustup` is often installed with `--no-modify-path`, so `.cargo\bin` is on neither the user nor the
machine `PATH` and a fresh shell cannot find `cargo`. Pick the line for your shell:

```bat
:: CMD — run this once per window (or use the persistent option below)
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
```

```bash
source scripts/rust-env.sh        # Git Bash / bash
.\scripts\rust-env.ps1            # PowerShell
```

CMD cannot run a `.ps1`, and `for /f`-style wrappers around it only capture its output without
exporting anything — the `set` above is what actually puts `cargo` on `PATH`. It is also all CMD
needs: `cargo build`/`cargo tauri` find the MSVC linker through the registry, so `INCLUDE`/`LIB` are
not required (verified by a full `--release` link with only this `set`).

To make it permanent instead of per-window, add `.cargo\bin` to the user `PATH` once. Run this in
PowerShell, not CMD:

```powershell
$bin = Join-Path $env:USERPROFILE '.cargo\bin'
$old = [Environment]::GetEnvironmentVariable('PATH', 'User')
if ($old -notlike "*$bin*") {
  [Environment]::SetEnvironmentVariable('PATH', "$old;$bin", 'User')
}
```

Open a new window afterwards. Avoid `setx PATH ...` here: it expands `%USERPROFILE%` in the existing
value (this machine's raw `PATH` is `REG_EXPAND_SZ` and contains it), and it silently truncates values
over 1024 characters.

`cargo tauri info` should report rustc, MSVC and WebView2. Installed components and disk usage:
[`docs/RUST-ENV.md`](docs/RUST-ENV.md).

## Build from source

Day-to-day development:

```bat
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
npm run tauri:sync
cd tauri-shell\src-tauri
cargo tauri dev
```

Releasing — **one command, two artifacts, one obvious folder**:

```bat
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
npm run tauri:bundle
```

```
release/
  AgentKeeper_<ver>_x64-setup.exe        NSIS installer
  AgentKeeper_<ver>_x64-portable.zip     install-free archive
```

`npm run tauri:bundle` runs `tauri:sync` → `tauri:core-bins` → `cargo tauri build` → staging, then
copies both artifacts into `release/` at the repository root and verifies each copy byte-for-byte. It
exits non-zero if either artifact is missing. `--no-build` stages what is already in `target/release`;
`--portable-only` skips the installer.

The portable archive holds `agentkeeper-shell.exe`, `agentkeeper-mcp.exe`, `ak-scan.exe` and a
short `README-PORTABLE.txt`; unzip and run, nothing is installed (no shortcut, no uninstall entry —
the app writes no Windows registry keys at all). Both forms share `%APPDATA%\AgentKeeper` for
settings, so set `AK_STORE_ROOT` to keep two copies apart.

The MCP server and the headless scanner live in a separate crate, `tauri-shell/core`, with **no
Tauri dependency** — so they link 114 crates instead of 311 and never pull in a WebView stack they
do not call. `cargo tauri build` only sees the binaries of the crate it builds, so they are staged
as sidecars for `bundle.externalBin` by `npm run tauri:core-bins` (which `tauri:bundle` runs for
you).

That arrangement is also what makes the sidecar possible at all. When the two binaries were
`[[bin]]` targets of the Tauri crate, the setup was circular: `tauri-build` runs as that crate's
build script for every target, so a clean `target/` could not build a sidecar the build script
insisted already existed. The core crate has no build script, so the sidecar is produced by
something that does not care.

There are no npm dependencies to install — `package.json` only declares the helper scripts.

`tauri:sync` is not optional: Tauri compiles `tauri-shell/dist` into the executable, and `dist` is
generated from `src/renderer` with exactly two controlled differences (`<html data-shell="tauri">`
and an extra `tauri-bridge.js`). `npm run tauri:check` fails when the two drift apart.

`dist/` is **not in git** — it is a build artefact, and a committed copy could silently be older
than `src/renderer` (the build does not check freshness, it just compiles whatever is in `dist/`).
`tauri.conf.json` therefore runs `copy-frontend.js` as `beforeBuildCommand` / `beforeDevCommand`, so
`cargo tauri build` and `cargo tauri dev` always regenerate it. A fresh clone with no `dist/` builds
correctly: the command creates it.

To verify the copy instead of writing it, use `npm run tauri:check` (or `npm run tauri:sync -- --check`).
A bare `npm run tauri:sync --check` runs `tauri:sync` with no arguments — npm only forwards flags after
`--`, so `--check` is dropped and the sync writes.

### Release artifacts

| | Installer | Portable |
|---|---|---|
| File | `release/AgentKeeper_<ver>_x64-setup.exe` | `release/AgentKeeper_<ver>_x64-portable.zip` |
| Size | 3.86 MiB | 7.81 MiB (21.3 MiB unpacked) |
| Install mode | `currentUser` → `%LOCALAPPDATA%\AgentKeeper`, no admin rights | none — unzip anywhere and run `agentkeeper-shell.exe` |
| Footprint | 21.4 MiB after install | 21.3 MiB in the folder |
| Uninstall | `uninstall.exe /S` | delete the folder |
| Frontend | embedded in the executable (no `dist/` on disk) | same |
| Cold start → window visible | 45–180 ms | 171–178 ms |

Silent install: `AgentKeeper_<ver>_x64-setup.exe /S /D=<absolute path>` (`/D=` last, unquoted).

Both artifacts are produced from one build; publishing the zip does not confuse the in-app updater,
which discards it on two independent rules (`-?portable\b` and the `.zip` archive pattern in
`core/update/version.rs`). Only the NSIS setup is ever offered as an update.

## Architecture

```
src/renderer/            native DOM UI (index.html · app.js · styles.css · i18n.js · engine-zh.js)
tauri-shell/dist/        generated copy of src/renderer, compiled into the executable (not in git)
tauri-shell/             Rust workspace
  core/                  agentkeeper-core — no Tauri dependency
    src/                 scan · assets · registry · adapters · store · keeper · mcp · security · update
    src/bin/             agentkeeper-mcp (stdio MCP server) · ak-scan (headless scan)
  src-tauri/             agentkeeper-shell — the Tauri app
    src/lib.rs           #[tauri::command] surface + window bootstrap
scripts/                 check · lint-renderer · ui-audit · tauri-i18n · adapter-table · package-release
release/                 build output: installer + portable zip (git-ignored)
docs/                    DESIGN.md · UPDATE.md · MCP-SERVER.md · RUST-ENV.md
```

`core::` never sees a Tauri type, which is why the same logic has three entry points: the GUI,
the MCP server and `ak-scan`.

### Safety model

- Frontend has no Node integration; only the named functions exposed by `tauri-bridge.js`.
- Two CSP layers (webview config and a stricter document-level meta tag); `connect-src 'none'`
  in the document layer.
- Paths are resolved from the registry — the renderer never passes a bare path (the only exception
  is "import project", which must be an existing directory).
- Writes go straight to the real agent directory through a transactional pipeline:
  validate → capture in memory → atomic write → journal. A failed write is rolled back to that
  capture; nothing is retained afterwards.
- Nothing is held back for approval: a write either lands in the real directory or is blocked by
  validation, and what the UI shows is exactly what gets written.
- Every create, update and delete is confirmed first, and the dialog lists the real target paths.
  Settings are exempt — they write to the app's own store, never to an agent directory.
- Tauri plugins are limited to `dialog` (pick a directory) and `opener` (reveal in Explorer).

## Testing

```bash
npm test                  # only the tests a change can affect — the default while iterating
npm run test:all          # every test (333), before a commit
npm test -- --list        # print what would run, run nothing
```

`npm test` reads `git status` and picks the test targets the changed files can break; `npm run test:all`
runs everything. Which script to run for which change, and what each one does, is in
[`docs/BUILDING.md`](docs/BUILDING.md).

The full suite directly:

```bash
cd tauri-shell
cargo test --workspace             # unit + integration, both crates
cargo test -p agentkeeper-core --test mcp   # real stdio MCP protocol regression
```

Static gates for the frontend and bridge (Node only, no browser, ≈0.5 s):

```bash
npm run check                    # residue · bridge · tree · renderer · ui · i18n · dist (8 gates)
npm run tauri:i18n               # Chinese coverage of Rust engine strings (CI runs this before building)
node scripts/check.js bridge     # or one gate only: residue / bridge / tree / renderer / ui / i18n / dist
```

Behaviour is owned by the Rust tests. Visual and keyboard acceptance needs a real WebView2 on an
interactive desktop, so it runs outside CI.

## Documentation

| Document | Contents |
|---|---|
| [`docs/DESIGN.md`](docs/DESIGN.md) | problem statement, architecture, write pipeline, adapter table, UI model |
| [`docs/UPDATE.md`](docs/UPDATE.md) | update pipeline: release layout, signature, proxy, installer |
| [`docs/MCP-SERVER.md`](docs/MCP-SERVER.md) | the 17 MCP tools and their schemas |
| [`docs/RUST-ENV.md`](docs/RUST-ENV.md) | local toolchain, components, disk usage |
| [`docs/BUILDING.md`](docs/BUILDING.md) | which script to run for which change; the two build profiles and why |

## Troubleshooting

| Symptom | Fix |
|---|---|
| `cargo: command not found` | `source scripts/rust-env.sh` (bash) or `. .\scripts\rust-env.ps1` (PowerShell) |
| `resource path binaries\agentkeeper-mcp-x86_64-pc-windows-msvc.exe doesn't exist` | the sidecar was not staged — run `npm run tauri:core-bins` before `cargo tauri build` |
| `npm run tauri:sync --check` still rewrote `dist/` | npm drops flags before `--`; use `npm run tauri:check` |
| `linker 'link.exe' not found` | MSVC environment not wired up — same commands; install "Desktop development with C++" |
| Window never appears | the host cannot initialise the Chromium OS sandbox; `src/lib.rs` sets `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` with `--no-sandbox --disable-gpu-sandbox` before the webview is created |
| Frontend change not visible | forgot `npm run tauri:sync`; `npm run tauri:check` reports stale `dist` |
| Dependency download stalls | the mirror is in `%USERPROFILE%\.cargo\config.toml`, not in the repo — see `docs/RUST-ENV.md` |
| A write "did nothing" | check *Operations* for the transaction: an `error` row means validation blocked it and nothing was written |
| MCP and GUI disagree | both sides must point at the same `AK_STORE_ROOT` |

## Contributing

- Issues and pull requests are welcome. Keep a change focused: one behaviour per PR.
- Run `npm run check` and `cargo test` before opening a PR; both must pass.
- Frontend edits go into `src/renderer/` only — `tauri-shell/dist/` is generated (not in git) and
  rebuilt by every `cargo tauri build` / `cargo tauri dev`.
- New strings need both languages: `src/renderer/i18n.js` (UI) and `src/renderer/engine-zh.js`
  (Rust engine sentences), or `npm run check` fails.
- Adding an agent touches two places: `core/adapters.rs` and `scripts/adapter-table.js`, then
  `npm run tauri:adapters`.
- The repository holds product files only: no audit/planning documents, no local absolute
  paths, no debug artefacts. Anything that describes how a change was made belongs in the PR,
  not in the tree.

## License

MIT — see [`LICENSE`](LICENSE).

Copyright (c) 2026 AgentKeeper.
