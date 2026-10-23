'use strict';
/**
 * Adapter table — the generator input for `core/adapters.rs`.
 *
 * SOURCE
 *   The canonical declaration of every supported agent: id, label, accent
 *   colour and the per-scope directories it reads. `npm run tauri:adapters`
 *   evaluates this file against a fake home and freezes the result into the
 *   `DECLARED_TABLE_JSON` / `RESOLVED_PATHS_JSON` constants that the Rust
 *   tests compare against, so the two declarations cannot drift.
 *
 * WHY IT STANDS ALONE
 *   The only consumer of this table is `scripts/gen-adapters-snapshots.js`,
 *   which freezes it into
 *   `DECLARED_TABLE_JSON` / `RESOLVED_PATHS_JSON` inside
 *   `tauri-shell/core/src/adapters.rs`. Nothing else may import it:
 *   this file exists to regenerate a snapshot, not to be an engine.
 *
 * WHEN YOU EDIT THIS FILE
 *   Re-run `npm run tauri:adapters` (scripts/gen-adapters-snapshots.js) IN THE
 *   SAME CHANGE. The Rust snapshot is the parity oracle for the `AGENTS` table
 *   in adapters.rs — an added/removed/renamed agent, a changed `home` template,
 *   or any edited skills/rules/memory/mcp/plugins list makes it stale, and a
 *   stale snapshot asserts the old table rather than what this file says.
 *
 * WHAT IS NOT PART OF THIS TABLE
 *   The `detected: (fs, p) => ...` closures: never invoked, and detection now
 *   lives in Rust (`adapters.rs` derives `installed` from `existsDir(home)`).
 *   `detected` was never part of the generated
 *   snapshot either, so dropping it leaves the snapshot byte-identical.
 */

const os = require('os');

const AGENTS = [
  {
    id: 'workbuddy',
    label: 'WorkBuddy',
    vendor: 'Tencent',
    family: 'workbuddy',
    accent: '#2f6bff',
    home: '~/.workbuddy',
    globals: {
      skills: ['skills'],
      plugins: ['plugins/cache', 'plugins/marketplaces'],
      rules: ['SOUL.md', 'IDENTITY.md', 'USER.md', 'BOOTSTRAP.md'],
      memory: ['MEMORY.md', 'memory'],
      mcp: ['mcp.json', 'mcp-approvals.json', 'mcp-tool-list.json']
    },
    projects: {
      marker: '.workbuddy',
      skills: ['.workbuddy/skills'],
      rules: ['.workbuddy/rules', 'AGENTS.md', 'CLAUDE.md'],
      memory: ['.workbuddy/memory', '.workbuddy/MEMORY.md'],
      mcp: ['.workbuddy/mcp.json'],
      plugins: ['.workbuddy/plugins']
    },
    pluginStateFiles: [
      'plugins/installed_plugins.json',
      'plugins/known_marketplaces.json',
      'plugins/configured-plugins.json'
    ],
    notes: 'Personal identity files live under ~/.workbuddy/<userId>-personal/'
  },
  {
    id: 'claude-code',
    label: 'Claude Code',
    vendor: 'Anthropic',
    family: 'claude',
    accent: '#d97757',
    home: '~/.claude',
    globals: {
      skills: ['skills'],
      plugins: ['plugins'],
      rules: ['CLAUDE.md', 'settings.json'],
      memory: ['CLAUDE.md'],
      mcp: ['.mcp.json', 'settings.json', 'mcp.json']
    },
    projects: {
      marker: '.claude',
      skills: ['.claude/skills'],
      rules: ['.claude/CLAUDE.md', 'CLAUDE.md', 'AGENTS.md'],
      memory: ['.claude/memory', 'CLAUDE.local.md'],
      mcp: ['.mcp.json', '.claude/settings.json'],
      plugins: ['.claude/plugins']
    }
  },
  {
    id: 'codex',
    label: 'Codex',
    vendor: 'OpenAI',
    family: 'codex',
    accent: '#10a37f',
    home: '~/.codex',
    globals: {
      skills: ['skills', 'prompts'],
      plugins: ['plugins'],
      rules: ['AGENTS.md'],
      memory: ['AGENTS.md', 'memories'],
      mcp: ['config.toml', 'mcp.json']
    },
    projects: {
      marker: '.codex',
      skills: ['.codex/skills', '.codex/prompts'],
      rules: ['AGENTS.md', '.codex/AGENTS.md'],
      memory: ['.codex/memories'],
      mcp: ['.codex/config.toml', '.codex/mcp.json']
    },
    formats: { toml: true }
  },
  {
    id: 'cursor',
    label: 'Cursor',
    vendor: 'Anysphere',
    family: 'cursor',
    accent: '#c9c9c9',
    home: '~/.cursor',
    globals: {
      skills: ['skills'],
      plugins: ['extensions'],
      rules: ['rules', 'rules.mdc'],
      memory: ['rules', 'memory'],
      mcp: ['mcp.json']
    },
    projects: {
      marker: '.cursor',
      skills: ['.cursor/skills'],
      rules: ['.cursor/rules', '.cursorrules'],
      memory: ['.cursor/memory'],
      mcp: ['.cursor/mcp.json']
    },
    notes: 'Cursor rules use .mdc with frontmatter'
  },
  {
    id: 'opencode',
    label: 'OpenCode',
    vendor: 'opencode-ai',
    family: 'agents-md',
    accent: '#7c5cff',
    home: '~/.opencode',
    globals: {
      skills: ['skills'],
      plugins: ['plugins'],
      rules: ['AGENTS.md'],
      memory: ['AGENTS.md'],
      mcp: ['opencode.json', 'mcp.json']
    },
    projects: {
      marker: '.opencode',
      skills: ['.opencode/skills', '.opencode/skill'],
      rules: ['AGENTS.md'],
      memory: ['.opencode/AGENTS.md'],
      mcp: ['.opencode/opencode.json']
    }
  },
  {
    id: 'gemini-cli',
    label: 'Gemini CLI',
    vendor: 'Google',
    family: 'gemini',
    accent: '#4285f4',
    home: '~/.gemini',
    globals: {
      skills: ['skills', 'extensions'],
      plugins: ['extensions'],
      rules: ['GEMINI.md'],
      memory: ['GEMINI.md', 'memory'],
      mcp: ['settings.json', 'mcp.json']
    },
    projects: {
      marker: '.gemini',
      skills: ['.gemini/skills', '.gemini/extensions'],
      rules: ['GEMINI.md', '.gemini/GEMINI.md'],
      memory: ['.gemini/memory'],
      mcp: ['.gemini/settings.json', '.gemini/mcp.json'],
      plugins: ['.gemini/extensions']
    }
  },
  {
    id: 'copilot',
    label: 'GitHub Copilot',
    vendor: 'GitHub',
    family: 'copilot',
    accent: '#6e40c9',
    home: '~/.config/github-copilot',
    globals: {
      skills: ['skills'],
      plugins: ['extensions'],
      rules: ['copilot-instructions.md'],
      memory: ['copilot-instructions.md'],
      mcp: ['mcp.json']
    },
    projects: {
      marker: '.github',
      skills: ['.github/skills', '.github/instructions'],
      rules: ['.github/copilot-instructions.md'],
      memory: ['.github/instructions'],
      mcp: ['.vscode/mcp.json']
    }
  },
  {
    id: 'windsurf',
    label: 'Windsurf',
    vendor: 'Codeium',
    family: 'windsurf',
    accent: '#0ba5ec',
    home: '~/.codeium/windsurf',
    globals: {
      skills: ['skills'],
      plugins: ['extensions'],
      rules: ['rules/global_rules.md'],
      memory: ['memories'],
      mcp: ['mcp_config.json']
    },
    projects: {
      marker: '.windsurf',
      skills: ['.windsurf/skills'],
      rules: ['.windsurf/rules.md', '.windsurfrules'],
      memory: ['.windsurf/memories'],
      mcp: ['.windsurf/mcp.json']
    }
  },
  {
    id: 'cline',
    label: 'Cline',
    vendor: 'cline',
    family: 'cline',
    accent: '#e5606a',
    home: '~/Documents/Cline',
    globals: {
      skills: ['Rules'],
      plugins: ['rules'],
      rules: ['rules'],
      memory: ['memories'],
      mcp: []
    },
    projects: {
      marker: '.clinerules',
      skills: ['.clinerules/skills'],
      rules: ['.clinerules', '.clinerules/workflows'],
      memory: ['.clinerules/memories'],
      mcp: ['.cline/mcp.json']
    }
  },
  {
    id: 'amp',
    label: 'Amp',
    vendor: 'Sourcegraph',
    family: 'agents-md',
    accent: '#f5345b',
    home: '~/.config/amp',
    globals: {
      skills: ['skills'],
      plugins: ['extensions'],
      rules: ['AGENTS.md'],
      memory: ['AGENTS.md'],
      mcp: ['settings.json']
    },
    projects: {
      marker: '.amp',
      skills: ['.amp/skills'],
      rules: ['AGENTS.md'],
      memory: ['.amp/AGENTS.md'],
      mcp: ['.vscode/mcp.json']
    }
  },
  {
    id: 'continue',
    label: 'Continue',
    vendor: 'continue',
    family: 'continue',
    accent: '#00a8e8',
    home: '~/.continue',
    globals: {
      skills: ['skills'],
      plugins: [],
      rules: ['rules'],
      memory: ['rules'],
      mcp: ['config.json', 'config.yaml']
    },
    projects: {
      marker: '.continue',
      skills: ['.continue/skills'],
      rules: ['.continue/rules'],
      memory: ['.continue/rules'],
      mcp: ['.continue/config.yaml']
    }
  },
  {
    id: 'roo',
    label: 'Roo Code',
    vendor: 'RooCodeInc',
    family: 'roo',
    accent: '#d95b39',
    home: '~/.roo',
    globals: {
      skills: ['skills'],
      plugins: ['extensions'],
      rules: ['rules'],
      memory: ['rules'],
      mcp: ['mcp_settings.json']
    },
    projects: {
      marker: '.roo',
      skills: ['.roo/skills'],
      rules: ['.roo/rules', '.roomodes'],
      memory: ['.roo/rules'],
      mcp: ['.roo/mcp.json']
    }
  },
  {
    id: 'goose',
    label: 'Goose',
    vendor: 'Block',
    family: 'goose',
    accent: '#e2762c',
    home: '~/.config/goose',
    globals: {
      skills: ['skills'],
      plugins: ['extensions'],
      rules: ['instructions.md'],
      memory: ['sessions'],
      mcp: ['config.yaml']
    },
    projects: {
      marker: '.goose',
      skills: ['.goose/skills'],
      rules: ['.goosehints'],
      memory: ['.goose/sessions'],
      mcp: ['.goose/config.yaml']
    }
  },
  {
    id: 'zed',
    label: 'Zed',
    vendor: 'Zed Industries',
    family: 'zed',
    accent: '#084ccf',
    home: '~/.config/zed',
    globals: {
      skills: ['skills'],
      plugins: ['extensions'],
      rules: ['rules'],
      memory: [],
      mcp: ['settings.json']
    },
    projects: {
      marker: '.zed',
      skills: ['.zed/skills'],
      rules: ['.rules', '.zed/rules'],
      memory: ['.zed/memory'],
      mcp: ['.zed/settings.json']
    }
  },
  {
    id: 'warp',
    label: 'Warp',
    vendor: 'Warp',
    family: 'warp',
    accent: '#22b0ff',
    home: '~/.warp',
    globals: {
      skills: [],
      plugins: [],
      rules: [],
      memory: ['memory'],
      mcp: []
    },
    projects: {
      marker: '.warp',
      skills: ['.warp/skills'],
      rules: ['.warp/WARP.md'],
      memory: ['.warp/memory'],
      mcp: ['.warp/mcp.json']
    }
  },
  {
    id: 'pi',
    label: 'Pi',
    vendor: 'pi',
    family: 'agents-shared',
    accent: '#8b5cf6',
    home: '~/.pi/agent',
    envOverrides: { home: 'PI_CODING_AGENT_DIR' },
    globals: {
      skills: ['skills'],
      plugins: [],
      rules: ['AGENTS.md'],
      memory: ['AGENTS.md'],
      mcp: ['mcp.json']
    },
    projects: {
      marker: '.pi',
      skills: ['.pi/skills'],
      rules: ['AGENTS.md'],
      memory: ['.pi/AGENTS.md'],
      mcp: ['.pi/mcp.json']
    }
  },
  {
    id: 'dsh',
    label: 'DeepSeek Harness',
    vendor: 'deepseek',
    family: 'agents-shared',
    accent: '#4d6bfe',
    home: '~/.dsh',
    envOverrides: { home: 'DSH_HOME' },
    globals: {
      skills: ['skills'],
      plugins: [],
      rules: ['AGENTS.md'],
      memory: ['AGENTS.md'],
      mcp: ['mcp.json']
    },
    projects: {
      marker: '.dsh',
      skills: ['.dsh/skills'],
      rules: ['AGENTS.md'],
      memory: ['.dsh/memories'],
      mcp: ['.dsh/mcp.json']
    }
  }
];

/** Cross-agent shared Agent Skills standard directory (~/.agents/skills). */
const SHARED_SKILLS_DIR = '~/.agents/skills';

/**
 * Expand a leading `~` in a path template (home / env token expansion).
 *
 * Only the tilde is substituted: the separator right behind it stays as
 * written, so `~/.claude` becomes `<home>/.claude`, never a normalised
 * `path.join(home, '.claude')`. That byte-level behaviour is asserted on the
 * Rust side by `resolve_home_matches_the_js_substitution_byte_for_byte`.
 *
 * Tilde expansion lives here only: the snapshot generator imports this exact
 * regex so the two cannot drift.
 *
 * os.homedir() is called per invocation (never captured at require time), so a
 * caller can monkey-patch os.homedir before requiring this module.
 */
const resolveHome = (token) => (!token ? null : token.replace(/^~(?=$|[\\/])/, os.homedir()));

/** Look up an adapter by id; an unknown id resolves to null, never a guess. */
function byId(id) {
  return AGENTS.find((a) => a.id === id) || null;
}

/**
 * Minimal surface for scripts/gen-adapters-snapshots.js.
 */
module.exports = { AGENTS, SHARED_SKILLS_DIR, resolveHome, byId };
