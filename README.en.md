# AgentKeeper

[简体中文](README.md) · [**English**](README.en.md)

![license](https://img.shields.io/badge/license-MIT-blue.svg)
![platform](https://img.shields.io/badge/platform-Windows%2010%2F11-0078D4.svg)

One local console for the **skills, MCP servers, plugins, rules and memory files** your coding
agents load. 5 asset kinds × 17 agents × global and project scope, in one window.

The shell is Tauri 2, the logic is Rust (`tauri-shell/core`), the UI is a zero-dependency native
DOM app (`src/renderer`) with no frontend build step.

## Features

- **Discover** — scans 17 agents plus the shared skills directory, global and project scope
- **See what actually applies** — which copy wins, and what a project scope shadows
- **Validate** — frontmatter / JSON / TOML / YAML checks plus an MCP security scan (prompt
  injection, leaked credentials)
- **Write safely** — snapshot → atomic write → full revert on a partial failure; every write asks
  for confirmation and lists the real paths it will touch
- **Distribute and audit** — copy assets to other agents or projects; append-only journal and
  baseline fingerprints that report silent edits
- **Agents manage themselves** — the same core ships as a 15-tool stdio MCP server

## Install

Download from [Releases](https://github.com/ntc0728/agentkeeper/releases):

| File | Notes |
|---|---|
| `AgentKeeper_<version>_x64-setup.exe` | per-user installer, no administrator rights |
| `AgentKeeper_<version>_x64-portable.zip` | no install, unzip and run |

Requires Windows 10/11 with the WebView2 runtime (bundled with Windows 11).

## Quick start

1. Open the app — it scans your agents and projects automatically
2. Search, filter, validate and edit on the **Assets** page
3. Every write asks first, a failed write reverts itself, and each change lands in **Activity**

## MCP server

`agentkeeper-mcp.exe` next to the app is the same core exposed over stdio MCP:

```json
{
  "mcpServers": {
    "agentkeeper": {
      "command": "C:\\Users\\<you>\\AppData\\Local\\AgentKeeper\\agentkeeper-mcp.exe"
    }
  }
}
```

15 tools cover listing, validation, writes, distribution and bundle migration, behind the same
validation and transactional write path as the GUI. See [docs/MCP-SERVER.md](docs/MCP-SERVER.md).

## Build from source

Requires Rust stable (`x86_64-pc-windows-msvc` with MSVC tools), Node.js (build scripts only) and
WebView2.

```bat
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
npm run tauri:sync
cd tauri-shell\src-tauri
cargo tauri dev
```

Installer + portable zip (written to `release\`):

```bash
npm run tauri:bundle
```

## Where the data lives

`%APPDATA%\AgentKeeper\agentkeeper` — settings, project list and the audit journal. Override with
the `AK_STORE_ROOT` environment variable. The install location and the data directory are
independent.

## Checks and tests

```bash
npm run check        # static gates: residue, bridge contract, renderer rules, i18n, dist
npm run test         # tests affected by the current change
npm run test:all     # everything
```

## Documentation

| Doc | Contents |
|---|---|
| [docs/DESIGN.md](docs/DESIGN.md) | asset model, scanner, write transaction, UI structure |
| [docs/MCP-SERVER.md](docs/MCP-SERVER.md) | MCP server setup |
| [docs/UPDATE.md](docs/UPDATE.md) | in-app updates (check, download, install, token) |
| [docs/CHANGELOG.md](docs/CHANGELOG.md) | release notes and the repo-local change record |
| [docs/BUILDING.md](docs/BUILDING.md) | build and release pipeline |
| [docs/RUST-ENV.md](docs/RUST-ENV.md) | Rust environment and components |

## License

[MIT](LICENSE)
