'use strict';
/**
 * 适配器表——`core/adapters.rs` 的生成器输入。
 *
 * 数据来源
 *   所有受支持 agent 的权威声明：id、标签、强调色以及它按作用域读取的
 *   目录。`npm run tauri:adapters`
 *   会在一个假的 home 目录下求值本文件，并把结果冻结进
 *   `DECLARED_TABLE_JSON` / `RESOLVED_PATHS_JSON` 常量，Rust
 *   测试正是拿它做比对，因此两份声明不会走偏。
 *
 * 为什么独立成文件
 *   这张表的唯一消费方是 `scripts/gen-adapters-snapshots.js`，
 *   它把表冻结进
 *   `tauri-shell/core/src/adapters.rs` 内的
 *   `DECLARED_TABLE_JSON` / `RESOLVED_PATHS_JSON`。其他任何东西都不得导入它：
 *   这个文件的存在是为了重新生成快照，而不是充当引擎。
 *
 * 修改本文件时
 *   在同一次改动里重新运行 `npm run tauri:adapters`
 *   （scripts/gen-adapters-snapshots.js）。Rust 快照是 adapters.rs 中 `AGENTS`
 *   表的一致性判据——新增/删除/重命名 agent、`home` 模板有变化，
 *   或 skills/rules/memory/mcp/plugins 列表有任何改动，都会让快照过期；
 *   而过期的快照断言的是旧表，而不是本文件所说的内容。
 *
 * 不属于这张表的内容
 *   `detected: (fs, p) => ...` 这些闭包：从未被调用，且检测逻辑现在
 *   位于 Rust 侧（`adapters.rs` 由 `existsDir(home)` 推导出 `installed`）。
 *   `detected` 本来也从未进入生成的
 *   快照，所以去掉它会让快照保持逐字节一致。
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

/** 跨 agent 共享的 Agent Skills 标准目录（~/.agents/skills）。 */
const SHARED_SKILLS_DIR = '~/.agents/skills';

/**
 * 展开路径模板中开头的 `~`（home / env 令牌展开）。
 *
 * 只替换波浪号：紧跟其后的分隔符保持原样，
 * 因此 `~/.claude` 会变成 `<home>/.claude`，绝不会变成被规范化的
 * `path.join(home, '.claude')`。这种逐字节的行为在
 * Rust 侧由 `resolve_home_matches_the_js_substitution_byte_for_byte` 断言。
 *
 * 波浪号展开只放在这里：快照生成器导入的正是这个
 * 正则，因此两者不会走偏。
 *
 * os.homedir() 每次调用时才求值（绝不在 require 时缓存），这样
 * 调用方可以在 require 本模块之前给 os.homedir 打补丁。
 */
const resolveHome = (token) => (!token ? null : token.replace(/^~(?=$|[\\/])/, os.homedir()));

/** 按 id 查找适配器；未知 id 解析为 null，绝不猜测。 */
function byId(id) {
  return AGENTS.find((a) => a.id === id) || null;
}

/**
 * 供 scripts/gen-adapters-snapshots.js 使用的最小导出面。
 */
module.exports = { AGENTS, SHARED_SKILLS_DIR, resolveHome, byId };
