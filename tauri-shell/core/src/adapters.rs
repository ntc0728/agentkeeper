//! Agent 适配器表——受支持 agent 的声明式清单。
//!
//! 共 17 个 agent，声明顺序固定：workbuddy、claude-code、
//! codex、cursor、opencode、gemini-cli、copilot、windsurf、cline、amp、
//! continue、roo、goose、zed、warp、pi、dsh。跨 agent 共享目录
//! （`~/.agents/skills`，即 [`SHARED_SKILLS_DIR`]）*不*在其中：它本身并不属于
//! 任何一个 agent，而 scan.rs 会把它当作独立的伪 agent（`shared`）暴露出来，
//! 所以它从不计入这 17 个里。
//!
//! 每个适配器都是纯数据。`installed` 恒等于 `existsDir(home)` ——
//! 声明里还保留着每个 agent 各自的 `detected` 谓词，但它从未被调用，而且它们
//! *并不*都是同一种检查：workbuddy 的那个还会额外要求
//! `~/.workbuddy/settings.json`。没有这层区分，就不要把它重新引进来。

pub struct Paths {
    pub skills: &'static [&'static str],
    pub plugins: &'static [&'static str],
    pub rules: &'static [&'static str],
    pub memory: &'static [&'static str],
    pub mcp: &'static [&'static str],
}

pub struct ProjectPaths {
    pub marker: &'static str,
    pub skills: &'static [&'static str],
    pub rules: &'static [&'static str],
    pub memory: &'static [&'static str],
    pub mcp: &'static [&'static str],
    /// `None` == 参考侧的适配器没有声明 `projects.plugins`。
    pub plugins: Option<&'static [&'static str]>,
}

pub struct Agent {
    pub id: &'static str,
    pub label: &'static str,
    pub vendor: &'static str,
    pub accent: &'static str,
    pub home: &'static str,
    pub notes: Option<&'static str>,
    pub globals: Paths,
    pub projects: ProjectPaths,
}

const NONE: &[&str] = &[];

pub static AGENTS: &[Agent] = &[
    Agent {
        id: "workbuddy",
        label: "WorkBuddy",
        vendor: "Tencent",
        accent: "#2f6bff",
        home: "~/.workbuddy",
        notes: Some("Personal identity files live under ~/.workbuddy/<userId>-personal/"),
        globals: Paths {
            skills: &["skills"],
            plugins: &["plugins/cache", "plugins/marketplaces"],
            rules: &["SOUL.md", "IDENTITY.md", "USER.md", "BOOTSTRAP.md"],
            memory: &["MEMORY.md", "memory"],
            mcp: &["mcp.json", "mcp-approvals.json", "mcp-tool-list.json"],
        },
        projects: ProjectPaths {
            marker: ".workbuddy",
            skills: &[".workbuddy/skills"],
            rules: &[".workbuddy/rules", "AGENTS.md", "CLAUDE.md"],
            memory: &[".workbuddy/memory", ".workbuddy/MEMORY.md"],
            mcp: &[".workbuddy/mcp.json"],
            plugins: Some(&[".workbuddy/plugins"]),
        },
    },
    Agent {
        id: "claude-code",
        label: "Claude Code",
        vendor: "Anthropic",
        accent: "#d97757",
        home: "~/.claude",
        notes: None,
        globals: Paths {
            skills: &["skills"],
            plugins: &["plugins"],
            rules: &["CLAUDE.md", "settings.json"],
            memory: &["CLAUDE.md"],
            mcp: &[".mcp.json", "settings.json", "mcp.json"],
        },
        projects: ProjectPaths {
            marker: ".claude",
            skills: &[".claude/skills"],
            rules: &[".claude/CLAUDE.md", "CLAUDE.md", "AGENTS.md"],
            memory: &[".claude/memory", "CLAUDE.local.md"],
            mcp: &[".mcp.json", ".claude/settings.json"],
            plugins: Some(&[".claude/plugins"]),
        },
    },
    Agent {
        id: "codex",
        label: "Codex",
        vendor: "OpenAI",
        accent: "#10a37f",
        home: "~/.codex",
        notes: None,
        globals: Paths {
            skills: &["skills", "prompts"],
            plugins: &["plugins"],
            rules: &["AGENTS.md"],
            memory: &["AGENTS.md", "memories"],
            mcp: &["config.toml", "mcp.json"],
        },
        projects: ProjectPaths {
            marker: ".codex",
            skills: &[".codex/skills", ".codex/prompts"],
            rules: &["AGENTS.md", ".codex/AGENTS.md"],
            memory: &[".codex/memories"],
            mcp: &[".codex/config.toml", ".codex/mcp.json"],
            plugins: None,
        },
    },
    Agent {
        id: "cursor",
        label: "Cursor",
        vendor: "Anysphere",
        accent: "#c9c9c9",
        home: "~/.cursor",
        notes: Some("Cursor rules use .mdc with frontmatter"),
        globals: Paths {
            skills: &["skills"],
            plugins: &["extensions"],
            rules: &["rules", "rules.mdc"],
            memory: &["rules", "memory"],
            mcp: &["mcp.json"],
        },
        projects: ProjectPaths {
            marker: ".cursor",
            skills: &[".cursor/skills"],
            rules: &[".cursor/rules", ".cursorrules"],
            memory: &[".cursor/memory"],
            mcp: &[".cursor/mcp.json"],
            plugins: None,
        },
    },
    Agent {
        id: "opencode",
        label: "OpenCode",
        vendor: "opencode-ai",
        accent: "#7c5cff",
        home: "~/.opencode",
        notes: None,
        globals: Paths {
            skills: &["skills"],
            plugins: &["plugins"],
            rules: &["AGENTS.md"],
            memory: &["AGENTS.md"],
            mcp: &["opencode.json", "mcp.json"],
        },
        projects: ProjectPaths {
            marker: ".opencode",
            skills: &[".opencode/skills", ".opencode/skill"],
            rules: &["AGENTS.md"],
            memory: &[".opencode/AGENTS.md"],
            mcp: &[".opencode/opencode.json"],
            plugins: None,
        },
    },
    Agent {
        id: "gemini-cli",
        label: "Gemini CLI",
        vendor: "Google",
        accent: "#4285f4",
        home: "~/.gemini",
        notes: None,
        globals: Paths {
            skills: &["skills", "extensions"],
            plugins: &["extensions"],
            rules: &["GEMINI.md"],
            memory: &["GEMINI.md", "memory"],
            mcp: &["settings.json", "mcp.json"],
        },
        projects: ProjectPaths {
            marker: ".gemini",
            skills: &[".gemini/skills", ".gemini/extensions"],
            rules: &["GEMINI.md", ".gemini/GEMINI.md"],
            memory: &[".gemini/memory"],
            mcp: &[".gemini/settings.json", ".gemini/mcp.json"],
            plugins: Some(&[".gemini/extensions"]),
        },
    },
    Agent {
        id: "copilot",
        label: "GitHub Copilot",
        vendor: "GitHub",
        accent: "#6e40c9",
        home: "~/.config/github-copilot",
        notes: None,
        globals: Paths {
            skills: &["skills"],
            plugins: &["extensions"],
            rules: &["copilot-instructions.md"],
            memory: &["copilot-instructions.md"],
            mcp: &["mcp.json"],
        },
        projects: ProjectPaths {
            marker: ".github",
            skills: &[".github/skills", ".github/instructions"],
            rules: &[".github/copilot-instructions.md"],
            memory: &[".github/instructions"],
            mcp: &[".vscode/mcp.json"],
            plugins: None,
        },
    },
    Agent {
        id: "windsurf",
        label: "Windsurf",
        vendor: "Codeium",
        accent: "#0ba5ec",
        home: "~/.codeium/windsurf",
        notes: None,
        globals: Paths {
            skills: &["skills"],
            plugins: &["extensions"],
            rules: &["rules/global_rules.md"],
            memory: &["memories"],
            mcp: &["mcp_config.json"],
        },
        projects: ProjectPaths {
            marker: ".windsurf",
            skills: &[".windsurf/skills"],
            rules: &[".windsurf/rules.md", ".windsurfrules"],
            memory: &[".windsurf/memories"],
            mcp: &[".windsurf/mcp.json"],
            plugins: None,
        },
    },
    Agent {
        id: "cline",
        label: "Cline",
        vendor: "cline",
        accent: "#e5606a",
        home: "~/Documents/Cline",
        notes: None,
        globals: Paths {
            skills: &["Rules"],
            plugins: &["rules"],
            rules: &["rules"],
            memory: &["memories"],
            mcp: NONE,
        },
        projects: ProjectPaths {
            marker: ".clinerules",
            skills: &[".clinerules/skills"],
            rules: &[".clinerules", ".clinerules/workflows"],
            memory: &[".clinerules/memories"],
            mcp: &[".cline/mcp.json"],
            plugins: None,
        },
    },
    Agent {
        id: "amp",
        label: "Amp",
        vendor: "Sourcegraph",
        accent: "#f5345b",
        home: "~/.config/amp",
        notes: None,
        globals: Paths {
            skills: &["skills"],
            plugins: &["extensions"],
            rules: &["AGENTS.md"],
            memory: &["AGENTS.md"],
            mcp: &["settings.json"],
        },
        projects: ProjectPaths {
            marker: ".amp",
            skills: &[".amp/skills"],
            rules: &["AGENTS.md"],
            memory: &[".amp/AGENTS.md"],
            mcp: &[".vscode/mcp.json"],
            plugins: None,
        },
    },
    Agent {
        id: "continue",
        label: "Continue",
        vendor: "continue",
        accent: "#00a8e8",
        home: "~/.continue",
        notes: None,
        globals: Paths {
            skills: &["skills"],
            plugins: NONE,
            rules: &["rules"],
            memory: &["rules"],
            mcp: &["config.json", "config.yaml"],
        },
        projects: ProjectPaths {
            marker: ".continue",
            skills: &[".continue/skills"],
            rules: &[".continue/rules"],
            memory: &[".continue/rules"],
            mcp: &[".continue/config.yaml"],
            plugins: None,
        },
    },
    Agent {
        id: "roo",
        label: "Roo Code",
        vendor: "RooCodeInc",
        accent: "#d95b39",
        home: "~/.roo",
        notes: None,
        globals: Paths {
            skills: &["skills"],
            plugins: &["extensions"],
            rules: &["rules"],
            memory: &["rules"],
            mcp: &["mcp_settings.json"],
        },
        projects: ProjectPaths {
            marker: ".roo",
            skills: &[".roo/skills"],
            rules: &[".roo/rules", ".roomodes"],
            memory: &[".roo/rules"],
            mcp: &[".roo/mcp.json"],
            plugins: None,
        },
    },
    Agent {
        id: "goose",
        label: "Goose",
        vendor: "Block",
        accent: "#e2762c",
        home: "~/.config/goose",
        notes: None,
        globals: Paths {
            skills: &["skills"],
            plugins: &["extensions"],
            rules: &["instructions.md"],
            memory: &["sessions"],
            mcp: &["config.yaml"],
        },
        projects: ProjectPaths {
            marker: ".goose",
            skills: &[".goose/skills"],
            rules: &[".goosehints"],
            memory: &[".goose/sessions"],
            mcp: &[".goose/config.yaml"],
            plugins: None,
        },
    },
    Agent {
        id: "zed",
        label: "Zed",
        vendor: "Zed Industries",
        accent: "#084ccf",
        home: "~/.config/zed",
        notes: None,
        globals: Paths {
            skills: &["skills"],
            plugins: &["extensions"],
            rules: &["rules"],
            memory: NONE,
            mcp: &["settings.json"],
        },
        projects: ProjectPaths {
            marker: ".zed",
            skills: &[".zed/skills"],
            rules: &[".rules", ".zed/rules"],
            memory: &[".zed/memory"],
            mcp: &[".zed/settings.json"],
            plugins: None,
        },
    },
    Agent {
        id: "warp",
        label: "Warp",
        vendor: "Warp",
        accent: "#22b0ff",
        home: "~/.warp",
        notes: None,
        globals: Paths {
            skills: NONE,
            plugins: NONE,
            rules: NONE,
            memory: &["memory"],
            mcp: NONE,
        },
        projects: ProjectPaths {
            marker: ".warp",
            skills: &[".warp/skills"],
            rules: &[".warp/WARP.md"],
            memory: &[".warp/memory"],
            mcp: &[".warp/mcp.json"],
            plugins: None,
        },
    },
    Agent {
        id: "pi",
        label: "Pi",
        vendor: "pi",
        accent: "#8b5cf6",
        home: "~/.pi/agent",
        notes: None,
        globals: Paths {
            skills: &["skills"],
            plugins: NONE,
            rules: &["AGENTS.md"],
            memory: &["AGENTS.md"],
            mcp: &["mcp.json"],
        },
        projects: ProjectPaths {
            marker: ".pi",
            skills: &[".pi/skills"],
            rules: &["AGENTS.md"],
            memory: &[".pi/AGENTS.md"],
            mcp: &[".pi/mcp.json"],
            plugins: None,
        },
    },
    Agent {
        id: "dsh",
        label: "DeepSeek Harness",
        vendor: "deepseek",
        accent: "#4d6bfe",
        home: "~/.dsh",
        notes: None,
        globals: Paths {
            skills: &["skills"],
            plugins: NONE,
            rules: &["AGENTS.md"],
            memory: &["AGENTS.md"],
            mcp: &["mcp.json"],
        },
        projects: ProjectPaths {
            marker: ".dsh",
            skills: &[".dsh/skills"],
            rules: &["AGENTS.md"],
            memory: &[".dsh/memories"],
            mcp: &[".dsh/mcp.json"],
            plugins: None,
        },
    },
];

/// 跨 agent 共享的 Agent Skills 标准目录。
pub const SHARED_SKILLS_DIR: &str = "~/.agents/skills";

/// `~` 记号解析（并且作为超集，也解析 `env:NAME`），与 `resolve_home()`
/// 的口径保持一致。
///
/// 参考实现是 `token.replace(/^~(?=$|[\\/])/, os.homedir())` —— 只替换波浪号，
/// 其余部分逐字节原样保留，所以结果是带正斜杠的 `C:\Users\me/.workbuddy`。
/// `agents[].home` 和每个资产的 `root` 里装的就是这种混合形态，因此必须精确
/// 复现；`norm_str` 会在由路径派生的字段里把它清理干净。
pub fn resolve_home(token: &str, user_home: &str) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    // `env:NAME` 展开已经实现，但今天走不到：没有任何适配器声明的 `home` 是以
    // `env:` 开头的。`envOverrides`（`pi` 和 `dsh` 上的 PI_CODING_AGENT_DIR /
    // DSH_HOME）虽然声明了，但从未被读取，所以不把它接上线并没有任何可观察的
    // 影响。要接上线，就得给 `Agent` 加上 `env_home: Option<&'static str>`，
    // 在 `scan_all`（scan.rs）/ `home_of`（keeper.rs）里让
    // `std::env::var(env_home)` 优先于 `resolve_home(agent.home, ..)`，并且重新
    // 生成快照（`npm run tauri:adapters`）——这些快照断言的是纯粹的 `~` 展开。
    if let Some(rest) = token.strip_prefix("env:") {
        return std::env::var(rest).ok();
    }
    if token == "~" {
        return Some(user_home.to_string());
    }
    if token.starts_with("~/") || token.starts_with("~\\") {
        return Some(format!(
            "{}{}",
            user_home.trim_end_matches(['\\', '/']),
            &token[1..]
        ));
    }
    Some(token.to_string())
}

/// `byId(agentId)` —— 未知 id 返回 `undefined`，绝不 panic。
///
/// 写入路径在每次变更之前都要先解析出 agent；一个不存在的 id 必须以
/// `cannot resolve … for <id>` 的形式暴露出来，而不是写到一个靠猜的路径上。
pub fn by_id(id: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.id == id)
}

/// `agent.family` — 每个适配器声明的分组标签。
///
/// 恰好只有一处行为依赖它：`Keeper.mcpBucketFor()` 把 copilot 的 server 写到
/// `servers` 下，其余所有 agent 则写到 `mcpServers` 下。让它在这里做成查表、
/// 而不是在 17 个字面量上都加一个字段，是为了让这张表保持为对路径的纯描述；
/// 等到出现第二个消费者那一天，再加真正的字段。
pub fn family(id: &str) -> &'static str {
    match id {
        "workbuddy" => "workbuddy",
        "claude-code" => "claude",
        "codex" => "codex",
        "cursor" => "cursor",
        "opencode" | "amp" => "agents-md",
        "gemini-cli" => "gemini",
        "copilot" => "copilot",
        "windsurf" => "windsurf",
        "cline" => "cline",
        "continue" => "continue",
        "roo" => "roo",
        "goose" => "goose",
        "zed" => "zed",
        "warp" => "warp",
        "pi" | "dsh" => "agents-shared",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    //! 针对声明的适配器表做快照测试。
    //!
    //! 这里的一切都是生成的：跑
    //!
    //!     npm run tauri:adapters      (scripts/gen-adapters-snapshots.js)
    //!
    //! 只要改过 scripts/adapter-table.js 就要重新生成。下面那两个 JSON 常量
    //! 就是该脚本的输出——用 os.homedir() 设为 FAKE_HOME 之后求值得到的适配器
    //! 表，外加针对它解析出来的每一条路径模板。绝不要手工编辑它们：手改过的
    //! 快照断言的是编辑者当时相信的东​西，而不是这张表实际说了什么。
    use super::*;
    use crate::model::join_native;
    use serde_json::Value;
    use std::path::Path;

    /// Fake user home / project root the snapshot below was taken with.
    /// Nothing here touches the real machine — every assertion compares
    /// strings produced by the two adapter tables for the same fake input.
    const FAKE_HOME: &str = "C:/ak-parity-home";
    const FAKE_PROJECT: &str = "C:/ak-parity-project";

    /// scripts/adapter-table.js loaded with os.homedir() === FAKE_HOME, plus the
    /// family and notes fields.
    const DECLARED_TABLE_JSON: &str = r##"{
 "fakeHome": "C:/ak-parity-home",
 "fakeProject": "C:/ak-parity-project",
 "sharedSkillsDir": "~/.agents/skills",
 "sharedSkills": "C:\\ak-parity-home\\.agents\\skills",
 "agents": [
  {
   "id": "workbuddy",
   "label": "WorkBuddy",
   "vendor": "Tencent",
   "family": "workbuddy",
   "accent": "#2f6bff",
   "home": "~/.workbuddy",
   "notes": "Personal identity files live under ~/.workbuddy/<userId>-personal/",
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [
     "plugins/cache",
     "plugins/marketplaces"
    ],
    "rules": [
     "SOUL.md",
     "IDENTITY.md",
     "USER.md",
     "BOOTSTRAP.md"
    ],
    "memory": [
     "MEMORY.md",
     "memory"
    ],
    "mcp": [
     "mcp.json",
     "mcp-approvals.json",
     "mcp-tool-list.json"
    ]
   },
   "projects": {
    "marker": ".workbuddy",
    "skills": [
     ".workbuddy/skills"
    ],
    "rules": [
     ".workbuddy/rules",
     "AGENTS.md",
     "CLAUDE.md"
    ],
    "memory": [
     ".workbuddy/memory",
     ".workbuddy/MEMORY.md"
    ],
    "mcp": [
     ".workbuddy/mcp.json"
    ],
    "plugins": [
     ".workbuddy/plugins"
    ]
   }
  },
  {
   "id": "claude-code",
   "label": "Claude Code",
   "vendor": "Anthropic",
   "family": "claude",
   "accent": "#d97757",
   "home": "~/.claude",
   "notes": null,
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [
     "plugins"
    ],
    "rules": [
     "CLAUDE.md",
     "settings.json"
    ],
    "memory": [
     "CLAUDE.md"
    ],
    "mcp": [
     ".mcp.json",
     "settings.json",
     "mcp.json"
    ]
   },
   "projects": {
    "marker": ".claude",
    "skills": [
     ".claude/skills"
    ],
    "rules": [
     ".claude/CLAUDE.md",
     "CLAUDE.md",
     "AGENTS.md"
    ],
    "memory": [
     ".claude/memory",
     "CLAUDE.local.md"
    ],
    "mcp": [
     ".mcp.json",
     ".claude/settings.json"
    ],
    "plugins": [
     ".claude/plugins"
    ]
   }
  },
  {
   "id": "codex",
   "label": "Codex",
   "vendor": "OpenAI",
   "family": "codex",
   "accent": "#10a37f",
   "home": "~/.codex",
   "notes": null,
   "globals": {
    "skills": [
     "skills",
     "prompts"
    ],
    "plugins": [
     "plugins"
    ],
    "rules": [
     "AGENTS.md"
    ],
    "memory": [
     "AGENTS.md",
     "memories"
    ],
    "mcp": [
     "config.toml",
     "mcp.json"
    ]
   },
   "projects": {
    "marker": ".codex",
    "skills": [
     ".codex/skills",
     ".codex/prompts"
    ],
    "rules": [
     "AGENTS.md",
     ".codex/AGENTS.md"
    ],
    "memory": [
     ".codex/memories"
    ],
    "mcp": [
     ".codex/config.toml",
     ".codex/mcp.json"
    ],
    "plugins": null
   }
  },
  {
   "id": "cursor",
   "label": "Cursor",
   "vendor": "Anysphere",
   "family": "cursor",
   "accent": "#c9c9c9",
   "home": "~/.cursor",
   "notes": "Cursor rules use .mdc with frontmatter",
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [
     "extensions"
    ],
    "rules": [
     "rules",
     "rules.mdc"
    ],
    "memory": [
     "rules",
     "memory"
    ],
    "mcp": [
     "mcp.json"
    ]
   },
   "projects": {
    "marker": ".cursor",
    "skills": [
     ".cursor/skills"
    ],
    "rules": [
     ".cursor/rules",
     ".cursorrules"
    ],
    "memory": [
     ".cursor/memory"
    ],
    "mcp": [
     ".cursor/mcp.json"
    ],
    "plugins": null
   }
  },
  {
   "id": "opencode",
   "label": "OpenCode",
   "vendor": "opencode-ai",
   "family": "agents-md",
   "accent": "#7c5cff",
   "home": "~/.opencode",
   "notes": null,
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [
     "plugins"
    ],
    "rules": [
     "AGENTS.md"
    ],
    "memory": [
     "AGENTS.md"
    ],
    "mcp": [
     "opencode.json",
     "mcp.json"
    ]
   },
   "projects": {
    "marker": ".opencode",
    "skills": [
     ".opencode/skills",
     ".opencode/skill"
    ],
    "rules": [
     "AGENTS.md"
    ],
    "memory": [
     ".opencode/AGENTS.md"
    ],
    "mcp": [
     ".opencode/opencode.json"
    ],
    "plugins": null
   }
  },
  {
   "id": "gemini-cli",
   "label": "Gemini CLI",
   "vendor": "Google",
   "family": "gemini",
   "accent": "#4285f4",
   "home": "~/.gemini",
   "notes": null,
   "globals": {
    "skills": [
     "skills",
     "extensions"
    ],
    "plugins": [
     "extensions"
    ],
    "rules": [
     "GEMINI.md"
    ],
    "memory": [
     "GEMINI.md",
     "memory"
    ],
    "mcp": [
     "settings.json",
     "mcp.json"
    ]
   },
   "projects": {
    "marker": ".gemini",
    "skills": [
     ".gemini/skills",
     ".gemini/extensions"
    ],
    "rules": [
     "GEMINI.md",
     ".gemini/GEMINI.md"
    ],
    "memory": [
     ".gemini/memory"
    ],
    "mcp": [
     ".gemini/settings.json",
     ".gemini/mcp.json"
    ],
    "plugins": [
     ".gemini/extensions"
    ]
   }
  },
  {
   "id": "copilot",
   "label": "GitHub Copilot",
   "vendor": "GitHub",
   "family": "copilot",
   "accent": "#6e40c9",
   "home": "~/.config/github-copilot",
   "notes": null,
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [
     "extensions"
    ],
    "rules": [
     "copilot-instructions.md"
    ],
    "memory": [
     "copilot-instructions.md"
    ],
    "mcp": [
     "mcp.json"
    ]
   },
   "projects": {
    "marker": ".github",
    "skills": [
     ".github/skills",
     ".github/instructions"
    ],
    "rules": [
     ".github/copilot-instructions.md"
    ],
    "memory": [
     ".github/instructions"
    ],
    "mcp": [
     ".vscode/mcp.json"
    ],
    "plugins": null
   }
  },
  {
   "id": "windsurf",
   "label": "Windsurf",
   "vendor": "Codeium",
   "family": "windsurf",
   "accent": "#0ba5ec",
   "home": "~/.codeium/windsurf",
   "notes": null,
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [
     "extensions"
    ],
    "rules": [
     "rules/global_rules.md"
    ],
    "memory": [
     "memories"
    ],
    "mcp": [
     "mcp_config.json"
    ]
   },
   "projects": {
    "marker": ".windsurf",
    "skills": [
     ".windsurf/skills"
    ],
    "rules": [
     ".windsurf/rules.md",
     ".windsurfrules"
    ],
    "memory": [
     ".windsurf/memories"
    ],
    "mcp": [
     ".windsurf/mcp.json"
    ],
    "plugins": null
   }
  },
  {
   "id": "cline",
   "label": "Cline",
   "vendor": "cline",
   "family": "cline",
   "accent": "#e5606a",
   "home": "~/Documents/Cline",
   "notes": null,
   "globals": {
    "skills": [
     "Rules"
    ],
    "plugins": [
     "rules"
    ],
    "rules": [
     "rules"
    ],
    "memory": [
     "memories"
    ],
    "mcp": []
   },
   "projects": {
    "marker": ".clinerules",
    "skills": [
     ".clinerules/skills"
    ],
    "rules": [
     ".clinerules",
     ".clinerules/workflows"
    ],
    "memory": [
     ".clinerules/memories"
    ],
    "mcp": [
     ".cline/mcp.json"
    ],
    "plugins": null
   }
  },
  {
   "id": "amp",
   "label": "Amp",
   "vendor": "Sourcegraph",
   "family": "agents-md",
   "accent": "#f5345b",
   "home": "~/.config/amp",
   "notes": null,
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [
     "extensions"
    ],
    "rules": [
     "AGENTS.md"
    ],
    "memory": [
     "AGENTS.md"
    ],
    "mcp": [
     "settings.json"
    ]
   },
   "projects": {
    "marker": ".amp",
    "skills": [
     ".amp/skills"
    ],
    "rules": [
     "AGENTS.md"
    ],
    "memory": [
     ".amp/AGENTS.md"
    ],
    "mcp": [
     ".vscode/mcp.json"
    ],
    "plugins": null
   }
  },
  {
   "id": "continue",
   "label": "Continue",
   "vendor": "continue",
   "family": "continue",
   "accent": "#00a8e8",
   "home": "~/.continue",
   "notes": null,
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [],
    "rules": [
     "rules"
    ],
    "memory": [
     "rules"
    ],
    "mcp": [
     "config.json",
     "config.yaml"
    ]
   },
   "projects": {
    "marker": ".continue",
    "skills": [
     ".continue/skills"
    ],
    "rules": [
     ".continue/rules"
    ],
    "memory": [
     ".continue/rules"
    ],
    "mcp": [
     ".continue/config.yaml"
    ],
    "plugins": null
   }
  },
  {
   "id": "roo",
   "label": "Roo Code",
   "vendor": "RooCodeInc",
   "family": "roo",
   "accent": "#d95b39",
   "home": "~/.roo",
   "notes": null,
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [
     "extensions"
    ],
    "rules": [
     "rules"
    ],
    "memory": [
     "rules"
    ],
    "mcp": [
     "mcp_settings.json"
    ]
   },
   "projects": {
    "marker": ".roo",
    "skills": [
     ".roo/skills"
    ],
    "rules": [
     ".roo/rules",
     ".roomodes"
    ],
    "memory": [
     ".roo/rules"
    ],
    "mcp": [
     ".roo/mcp.json"
    ],
    "plugins": null
   }
  },
  {
   "id": "goose",
   "label": "Goose",
   "vendor": "Block",
   "family": "goose",
   "accent": "#e2762c",
   "home": "~/.config/goose",
   "notes": null,
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [
     "extensions"
    ],
    "rules": [
     "instructions.md"
    ],
    "memory": [
     "sessions"
    ],
    "mcp": [
     "config.yaml"
    ]
   },
   "projects": {
    "marker": ".goose",
    "skills": [
     ".goose/skills"
    ],
    "rules": [
     ".goosehints"
    ],
    "memory": [
     ".goose/sessions"
    ],
    "mcp": [
     ".goose/config.yaml"
    ],
    "plugins": null
   }
  },
  {
   "id": "zed",
   "label": "Zed",
   "vendor": "Zed Industries",
   "family": "zed",
   "accent": "#084ccf",
   "home": "~/.config/zed",
   "notes": null,
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [
     "extensions"
    ],
    "rules": [
     "rules"
    ],
    "memory": [],
    "mcp": [
     "settings.json"
    ]
   },
   "projects": {
    "marker": ".zed",
    "skills": [
     ".zed/skills"
    ],
    "rules": [
     ".rules",
     ".zed/rules"
    ],
    "memory": [
     ".zed/memory"
    ],
    "mcp": [
     ".zed/settings.json"
    ],
    "plugins": null
   }
  },
  {
   "id": "warp",
   "label": "Warp",
   "vendor": "Warp",
   "family": "warp",
   "accent": "#22b0ff",
   "home": "~/.warp",
   "notes": null,
   "globals": {
    "skills": [],
    "plugins": [],
    "rules": [],
    "memory": [
     "memory"
    ],
    "mcp": []
   },
   "projects": {
    "marker": ".warp",
    "skills": [
     ".warp/skills"
    ],
    "rules": [
     ".warp/WARP.md"
    ],
    "memory": [
     ".warp/memory"
    ],
    "mcp": [
     ".warp/mcp.json"
    ],
    "plugins": null
   }
  },
  {
   "id": "pi",
   "label": "Pi",
   "vendor": "pi",
   "family": "agents-shared",
   "accent": "#8b5cf6",
   "home": "~/.pi/agent",
   "notes": null,
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [],
    "rules": [
     "AGENTS.md"
    ],
    "memory": [
     "AGENTS.md"
    ],
    "mcp": [
     "mcp.json"
    ]
   },
   "projects": {
    "marker": ".pi",
    "skills": [
     ".pi/skills"
    ],
    "rules": [
     "AGENTS.md"
    ],
    "memory": [
     ".pi/AGENTS.md"
    ],
    "mcp": [
     ".pi/mcp.json"
    ],
    "plugins": null
   }
  },
  {
   "id": "dsh",
   "label": "DeepSeek Harness",
   "vendor": "deepseek",
   "family": "agents-shared",
   "accent": "#4d6bfe",
   "home": "~/.dsh",
   "notes": null,
   "globals": {
    "skills": [
     "skills"
    ],
    "plugins": [],
    "rules": [
     "AGENTS.md"
    ],
    "memory": [
     "AGENTS.md"
    ],
    "mcp": [
     "mcp.json"
    ]
   },
   "projects": {
    "marker": ".dsh",
    "skills": [
     ".dsh/skills"
    ],
    "rules": [
     "AGENTS.md"
    ],
    "memory": [
     ".dsh/memories"
    ],
    "mcp": [
     ".dsh/mcp.json"
    ],
    "plugins": null
   }
  }
 ]
}"##;

    /// Every path template of every adapter, resolved on the reference side:
    /// path.resolve(home, rel) for globals and path.resolve(proj.dir, rel) for
    /// projects, with home coming from resolveHome() in
    /// scripts/adapter-table.js.
    const RESOLVED_PATHS_JSON: &str = r##"{
  "workbuddy": {"home":"C:/ak-parity-home/.workbuddy","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.workbuddy\\skills","globals.plugins:plugins/cache":"C:\\ak-parity-home\\.workbuddy\\plugins\\cache","globals.plugins:plugins/marketplaces":"C:\\ak-parity-home\\.workbuddy\\plugins\\marketplaces","globals.rules:SOUL.md":"C:\\ak-parity-home\\.workbuddy\\SOUL.md","globals.rules:IDENTITY.md":"C:\\ak-parity-home\\.workbuddy\\IDENTITY.md","globals.rules:USER.md":"C:\\ak-parity-home\\.workbuddy\\USER.md","globals.rules:BOOTSTRAP.md":"C:\\ak-parity-home\\.workbuddy\\BOOTSTRAP.md","globals.memory:MEMORY.md":"C:\\ak-parity-home\\.workbuddy\\MEMORY.md","globals.memory:memory":"C:\\ak-parity-home\\.workbuddy\\memory","globals.mcp:mcp.json":"C:\\ak-parity-home\\.workbuddy\\mcp.json","globals.mcp:mcp-approvals.json":"C:\\ak-parity-home\\.workbuddy\\mcp-approvals.json","globals.mcp:mcp-tool-list.json":"C:\\ak-parity-home\\.workbuddy\\mcp-tool-list.json"},"projectPaths":{"projects.skills:.workbuddy/skills":"C:\\ak-parity-project\\.workbuddy\\skills","projects.rules:.workbuddy/rules":"C:\\ak-parity-project\\.workbuddy\\rules","projects.rules:AGENTS.md":"C:\\ak-parity-project\\AGENTS.md","projects.rules:CLAUDE.md":"C:\\ak-parity-project\\CLAUDE.md","projects.memory:.workbuddy/memory":"C:\\ak-parity-project\\.workbuddy\\memory","projects.memory:.workbuddy/MEMORY.md":"C:\\ak-parity-project\\.workbuddy\\MEMORY.md","projects.mcp:.workbuddy/mcp.json":"C:\\ak-parity-project\\.workbuddy\\mcp.json","projects.plugins:.workbuddy/plugins":"C:\\ak-parity-project\\.workbuddy\\plugins"}},
  "claude-code": {"home":"C:/ak-parity-home/.claude","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.claude\\skills","globals.plugins:plugins":"C:\\ak-parity-home\\.claude\\plugins","globals.rules:CLAUDE.md":"C:\\ak-parity-home\\.claude\\CLAUDE.md","globals.rules:settings.json":"C:\\ak-parity-home\\.claude\\settings.json","globals.memory:CLAUDE.md":"C:\\ak-parity-home\\.claude\\CLAUDE.md","globals.mcp:.mcp.json":"C:\\ak-parity-home\\.claude\\.mcp.json","globals.mcp:settings.json":"C:\\ak-parity-home\\.claude\\settings.json","globals.mcp:mcp.json":"C:\\ak-parity-home\\.claude\\mcp.json"},"projectPaths":{"projects.skills:.claude/skills":"C:\\ak-parity-project\\.claude\\skills","projects.rules:.claude/CLAUDE.md":"C:\\ak-parity-project\\.claude\\CLAUDE.md","projects.rules:CLAUDE.md":"C:\\ak-parity-project\\CLAUDE.md","projects.rules:AGENTS.md":"C:\\ak-parity-project\\AGENTS.md","projects.memory:.claude/memory":"C:\\ak-parity-project\\.claude\\memory","projects.memory:CLAUDE.local.md":"C:\\ak-parity-project\\CLAUDE.local.md","projects.mcp:.mcp.json":"C:\\ak-parity-project\\.mcp.json","projects.mcp:.claude/settings.json":"C:\\ak-parity-project\\.claude\\settings.json","projects.plugins:.claude/plugins":"C:\\ak-parity-project\\.claude\\plugins"}},
  "codex": {"home":"C:/ak-parity-home/.codex","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.codex\\skills","globals.skills:prompts":"C:\\ak-parity-home\\.codex\\prompts","globals.plugins:plugins":"C:\\ak-parity-home\\.codex\\plugins","globals.rules:AGENTS.md":"C:\\ak-parity-home\\.codex\\AGENTS.md","globals.memory:AGENTS.md":"C:\\ak-parity-home\\.codex\\AGENTS.md","globals.memory:memories":"C:\\ak-parity-home\\.codex\\memories","globals.mcp:config.toml":"C:\\ak-parity-home\\.codex\\config.toml","globals.mcp:mcp.json":"C:\\ak-parity-home\\.codex\\mcp.json"},"projectPaths":{"projects.skills:.codex/skills":"C:\\ak-parity-project\\.codex\\skills","projects.skills:.codex/prompts":"C:\\ak-parity-project\\.codex\\prompts","projects.rules:AGENTS.md":"C:\\ak-parity-project\\AGENTS.md","projects.rules:.codex/AGENTS.md":"C:\\ak-parity-project\\.codex\\AGENTS.md","projects.memory:.codex/memories":"C:\\ak-parity-project\\.codex\\memories","projects.mcp:.codex/config.toml":"C:\\ak-parity-project\\.codex\\config.toml","projects.mcp:.codex/mcp.json":"C:\\ak-parity-project\\.codex\\mcp.json"}},
  "cursor": {"home":"C:/ak-parity-home/.cursor","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.cursor\\skills","globals.plugins:extensions":"C:\\ak-parity-home\\.cursor\\extensions","globals.rules:rules":"C:\\ak-parity-home\\.cursor\\rules","globals.rules:rules.mdc":"C:\\ak-parity-home\\.cursor\\rules.mdc","globals.memory:rules":"C:\\ak-parity-home\\.cursor\\rules","globals.memory:memory":"C:\\ak-parity-home\\.cursor\\memory","globals.mcp:mcp.json":"C:\\ak-parity-home\\.cursor\\mcp.json"},"projectPaths":{"projects.skills:.cursor/skills":"C:\\ak-parity-project\\.cursor\\skills","projects.rules:.cursor/rules":"C:\\ak-parity-project\\.cursor\\rules","projects.rules:.cursorrules":"C:\\ak-parity-project\\.cursorrules","projects.memory:.cursor/memory":"C:\\ak-parity-project\\.cursor\\memory","projects.mcp:.cursor/mcp.json":"C:\\ak-parity-project\\.cursor\\mcp.json"}},
  "opencode": {"home":"C:/ak-parity-home/.opencode","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.opencode\\skills","globals.plugins:plugins":"C:\\ak-parity-home\\.opencode\\plugins","globals.rules:AGENTS.md":"C:\\ak-parity-home\\.opencode\\AGENTS.md","globals.memory:AGENTS.md":"C:\\ak-parity-home\\.opencode\\AGENTS.md","globals.mcp:opencode.json":"C:\\ak-parity-home\\.opencode\\opencode.json","globals.mcp:mcp.json":"C:\\ak-parity-home\\.opencode\\mcp.json"},"projectPaths":{"projects.skills:.opencode/skills":"C:\\ak-parity-project\\.opencode\\skills","projects.skills:.opencode/skill":"C:\\ak-parity-project\\.opencode\\skill","projects.rules:AGENTS.md":"C:\\ak-parity-project\\AGENTS.md","projects.memory:.opencode/AGENTS.md":"C:\\ak-parity-project\\.opencode\\AGENTS.md","projects.mcp:.opencode/opencode.json":"C:\\ak-parity-project\\.opencode\\opencode.json"}},
  "gemini-cli": {"home":"C:/ak-parity-home/.gemini","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.gemini\\skills","globals.skills:extensions":"C:\\ak-parity-home\\.gemini\\extensions","globals.plugins:extensions":"C:\\ak-parity-home\\.gemini\\extensions","globals.rules:GEMINI.md":"C:\\ak-parity-home\\.gemini\\GEMINI.md","globals.memory:GEMINI.md":"C:\\ak-parity-home\\.gemini\\GEMINI.md","globals.memory:memory":"C:\\ak-parity-home\\.gemini\\memory","globals.mcp:settings.json":"C:\\ak-parity-home\\.gemini\\settings.json","globals.mcp:mcp.json":"C:\\ak-parity-home\\.gemini\\mcp.json"},"projectPaths":{"projects.skills:.gemini/skills":"C:\\ak-parity-project\\.gemini\\skills","projects.skills:.gemini/extensions":"C:\\ak-parity-project\\.gemini\\extensions","projects.rules:GEMINI.md":"C:\\ak-parity-project\\GEMINI.md","projects.rules:.gemini/GEMINI.md":"C:\\ak-parity-project\\.gemini\\GEMINI.md","projects.memory:.gemini/memory":"C:\\ak-parity-project\\.gemini\\memory","projects.mcp:.gemini/settings.json":"C:\\ak-parity-project\\.gemini\\settings.json","projects.mcp:.gemini/mcp.json":"C:\\ak-parity-project\\.gemini\\mcp.json","projects.plugins:.gemini/extensions":"C:\\ak-parity-project\\.gemini\\extensions"}},
  "copilot": {"home":"C:/ak-parity-home/.config/github-copilot","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.config\\github-copilot\\skills","globals.plugins:extensions":"C:\\ak-parity-home\\.config\\github-copilot\\extensions","globals.rules:copilot-instructions.md":"C:\\ak-parity-home\\.config\\github-copilot\\copilot-instructions.md","globals.memory:copilot-instructions.md":"C:\\ak-parity-home\\.config\\github-copilot\\copilot-instructions.md","globals.mcp:mcp.json":"C:\\ak-parity-home\\.config\\github-copilot\\mcp.json"},"projectPaths":{"projects.skills:.github/skills":"C:\\ak-parity-project\\.github\\skills","projects.skills:.github/instructions":"C:\\ak-parity-project\\.github\\instructions","projects.rules:.github/copilot-instructions.md":"C:\\ak-parity-project\\.github\\copilot-instructions.md","projects.memory:.github/instructions":"C:\\ak-parity-project\\.github\\instructions","projects.mcp:.vscode/mcp.json":"C:\\ak-parity-project\\.vscode\\mcp.json"}},
  "windsurf": {"home":"C:/ak-parity-home/.codeium/windsurf","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.codeium\\windsurf\\skills","globals.plugins:extensions":"C:\\ak-parity-home\\.codeium\\windsurf\\extensions","globals.rules:rules/global_rules.md":"C:\\ak-parity-home\\.codeium\\windsurf\\rules\\global_rules.md","globals.memory:memories":"C:\\ak-parity-home\\.codeium\\windsurf\\memories","globals.mcp:mcp_config.json":"C:\\ak-parity-home\\.codeium\\windsurf\\mcp_config.json"},"projectPaths":{"projects.skills:.windsurf/skills":"C:\\ak-parity-project\\.windsurf\\skills","projects.rules:.windsurf/rules.md":"C:\\ak-parity-project\\.windsurf\\rules.md","projects.rules:.windsurfrules":"C:\\ak-parity-project\\.windsurfrules","projects.memory:.windsurf/memories":"C:\\ak-parity-project\\.windsurf\\memories","projects.mcp:.windsurf/mcp.json":"C:\\ak-parity-project\\.windsurf\\mcp.json"}},
  "cline": {"home":"C:/ak-parity-home/Documents/Cline","globalPaths":{"globals.skills:Rules":"C:\\ak-parity-home\\Documents\\Cline\\Rules","globals.plugins:rules":"C:\\ak-parity-home\\Documents\\Cline\\rules","globals.rules:rules":"C:\\ak-parity-home\\Documents\\Cline\\rules","globals.memory:memories":"C:\\ak-parity-home\\Documents\\Cline\\memories"},"projectPaths":{"projects.skills:.clinerules/skills":"C:\\ak-parity-project\\.clinerules\\skills","projects.rules:.clinerules":"C:\\ak-parity-project\\.clinerules","projects.rules:.clinerules/workflows":"C:\\ak-parity-project\\.clinerules\\workflows","projects.memory:.clinerules/memories":"C:\\ak-parity-project\\.clinerules\\memories","projects.mcp:.cline/mcp.json":"C:\\ak-parity-project\\.cline\\mcp.json"}},
  "amp": {"home":"C:/ak-parity-home/.config/amp","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.config\\amp\\skills","globals.plugins:extensions":"C:\\ak-parity-home\\.config\\amp\\extensions","globals.rules:AGENTS.md":"C:\\ak-parity-home\\.config\\amp\\AGENTS.md","globals.memory:AGENTS.md":"C:\\ak-parity-home\\.config\\amp\\AGENTS.md","globals.mcp:settings.json":"C:\\ak-parity-home\\.config\\amp\\settings.json"},"projectPaths":{"projects.skills:.amp/skills":"C:\\ak-parity-project\\.amp\\skills","projects.rules:AGENTS.md":"C:\\ak-parity-project\\AGENTS.md","projects.memory:.amp/AGENTS.md":"C:\\ak-parity-project\\.amp\\AGENTS.md","projects.mcp:.vscode/mcp.json":"C:\\ak-parity-project\\.vscode\\mcp.json"}},
  "continue": {"home":"C:/ak-parity-home/.continue","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.continue\\skills","globals.rules:rules":"C:\\ak-parity-home\\.continue\\rules","globals.memory:rules":"C:\\ak-parity-home\\.continue\\rules","globals.mcp:config.json":"C:\\ak-parity-home\\.continue\\config.json","globals.mcp:config.yaml":"C:\\ak-parity-home\\.continue\\config.yaml"},"projectPaths":{"projects.skills:.continue/skills":"C:\\ak-parity-project\\.continue\\skills","projects.rules:.continue/rules":"C:\\ak-parity-project\\.continue\\rules","projects.memory:.continue/rules":"C:\\ak-parity-project\\.continue\\rules","projects.mcp:.continue/config.yaml":"C:\\ak-parity-project\\.continue\\config.yaml"}},
  "roo": {"home":"C:/ak-parity-home/.roo","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.roo\\skills","globals.plugins:extensions":"C:\\ak-parity-home\\.roo\\extensions","globals.rules:rules":"C:\\ak-parity-home\\.roo\\rules","globals.memory:rules":"C:\\ak-parity-home\\.roo\\rules","globals.mcp:mcp_settings.json":"C:\\ak-parity-home\\.roo\\mcp_settings.json"},"projectPaths":{"projects.skills:.roo/skills":"C:\\ak-parity-project\\.roo\\skills","projects.rules:.roo/rules":"C:\\ak-parity-project\\.roo\\rules","projects.rules:.roomodes":"C:\\ak-parity-project\\.roomodes","projects.memory:.roo/rules":"C:\\ak-parity-project\\.roo\\rules","projects.mcp:.roo/mcp.json":"C:\\ak-parity-project\\.roo\\mcp.json"}},
  "goose": {"home":"C:/ak-parity-home/.config/goose","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.config\\goose\\skills","globals.plugins:extensions":"C:\\ak-parity-home\\.config\\goose\\extensions","globals.rules:instructions.md":"C:\\ak-parity-home\\.config\\goose\\instructions.md","globals.memory:sessions":"C:\\ak-parity-home\\.config\\goose\\sessions","globals.mcp:config.yaml":"C:\\ak-parity-home\\.config\\goose\\config.yaml"},"projectPaths":{"projects.skills:.goose/skills":"C:\\ak-parity-project\\.goose\\skills","projects.rules:.goosehints":"C:\\ak-parity-project\\.goosehints","projects.memory:.goose/sessions":"C:\\ak-parity-project\\.goose\\sessions","projects.mcp:.goose/config.yaml":"C:\\ak-parity-project\\.goose\\config.yaml"}},
  "zed": {"home":"C:/ak-parity-home/.config/zed","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.config\\zed\\skills","globals.plugins:extensions":"C:\\ak-parity-home\\.config\\zed\\extensions","globals.rules:rules":"C:\\ak-parity-home\\.config\\zed\\rules","globals.mcp:settings.json":"C:\\ak-parity-home\\.config\\zed\\settings.json"},"projectPaths":{"projects.skills:.zed/skills":"C:\\ak-parity-project\\.zed\\skills","projects.rules:.rules":"C:\\ak-parity-project\\.rules","projects.rules:.zed/rules":"C:\\ak-parity-project\\.zed\\rules","projects.memory:.zed/memory":"C:\\ak-parity-project\\.zed\\memory","projects.mcp:.zed/settings.json":"C:\\ak-parity-project\\.zed\\settings.json"}},
  "warp": {"home":"C:/ak-parity-home/.warp","globalPaths":{"globals.memory:memory":"C:\\ak-parity-home\\.warp\\memory"},"projectPaths":{"projects.skills:.warp/skills":"C:\\ak-parity-project\\.warp\\skills","projects.rules:.warp/WARP.md":"C:\\ak-parity-project\\.warp\\WARP.md","projects.memory:.warp/memory":"C:\\ak-parity-project\\.warp\\memory","projects.mcp:.warp/mcp.json":"C:\\ak-parity-project\\.warp\\mcp.json"}},
  "pi": {"home":"C:/ak-parity-home/.pi/agent","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.pi\\agent\\skills","globals.rules:AGENTS.md":"C:\\ak-parity-home\\.pi\\agent\\AGENTS.md","globals.memory:AGENTS.md":"C:\\ak-parity-home\\.pi\\agent\\AGENTS.md","globals.mcp:mcp.json":"C:\\ak-parity-home\\.pi\\agent\\mcp.json"},"projectPaths":{"projects.skills:.pi/skills":"C:\\ak-parity-project\\.pi\\skills","projects.rules:AGENTS.md":"C:\\ak-parity-project\\AGENTS.md","projects.memory:.pi/AGENTS.md":"C:\\ak-parity-project\\.pi\\AGENTS.md","projects.mcp:.pi/mcp.json":"C:\\ak-parity-project\\.pi\\mcp.json"}},
  "dsh": {"home":"C:/ak-parity-home/.dsh","globalPaths":{"globals.skills:skills":"C:\\ak-parity-home\\.dsh\\skills","globals.rules:AGENTS.md":"C:\\ak-parity-home\\.dsh\\AGENTS.md","globals.memory:AGENTS.md":"C:\\ak-parity-home\\.dsh\\AGENTS.md","globals.mcp:mcp.json":"C:\\ak-parity-home\\.dsh\\mcp.json"},"projectPaths":{"projects.skills:.dsh/skills":"C:\\ak-parity-project\\.dsh\\skills","projects.rules:AGENTS.md":"C:\\ak-parity-project\\AGENTS.md","projects.memory:.dsh/memories":"C:\\ak-parity-project\\.dsh\\memories","projects.mcp:.dsh/mcp.json":"C:\\ak-parity-project\\.dsh\\mcp.json"}}
}"##;

    fn declared_table_json() -> Value {
        serde_json::from_str(DECLARED_TABLE_JSON).expect("adapter snapshot must be valid JSON")
    }

    fn resolved_paths_json() -> Value {
        serde_json::from_str(RESOLVED_PATHS_JSON).expect("JS path snapshot must be valid JSON")
    }

    fn declared_agents_json() -> Vec<Value> {
        match declared_table_json()
            .get("agents")
            .and_then(|v| v.as_array())
        {
            Some(a) => a.clone(),
            None => panic!("adapter snapshot has no agents array"),
        }
    }

    fn strs(v: &Value) -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .map(|x| x.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn list(v: &[&'static str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    /// Compare two paths the way the filesystem does. path.resolve rewrites
    /// every separator to the native one, while this implementation keeps the slash that
    /// resolve_home preserved after the tilde, so the raw strings can differ in
    /// separator only. What must match is the directory they name.
    fn same_path(js: &str, rust: &str) -> bool {
        fn norm(s: &str) -> String {
            s.replace('\\', "/")
        }
        norm(js) == norm(rust)
    }

    /// 17 adapters, declaration order preserved, every field identical.
    #[test]
    fn the_declared_table_and_the_rust_one_agree_field_for_field() {
        let js = declared_agents_json();
        assert_eq!(
            js.len(),
            AGENTS.len(),
            "snapshot declares {} adapters, Rust declares {}",
            js.len(),
            AGENTS.len()
        );

        for (i, j) in js.iter().enumerate() {
            let r = &AGENTS[i];
            let id = j["id"].as_str().unwrap_or_default();
            assert_eq!(r.id, id, "agent[{}].id", i);
            assert_eq!(
                r.label,
                j["label"].as_str().unwrap_or_default(),
                "{}: label",
                id
            );
            assert_eq!(
                r.vendor,
                j["vendor"].as_str().unwrap_or_default(),
                "{}: vendor",
                id
            );
            assert_eq!(
                r.accent,
                j["accent"].as_str().unwrap_or_default(),
                "{}: accent",
                id
            );
            assert_eq!(
                r.home,
                j["home"].as_str().unwrap_or_default(),
                "{}: home",
                id
            );

            // notes is undefined on every adapter but two.
            let declared_notes = j["notes"].as_str().map(|s| s.to_string());
            assert_eq!(
                r.notes.map(|s| s.to_string()),
                declared_notes,
                "{}: notes",
                id
            );

            // family is a plain field; this implementation derives it from the id.
            assert_eq!(
                family(r.id),
                j["family"].as_str().unwrap_or_default(),
                "{}: family",
                id
            );

            let g = &j["globals"];
            assert_eq!(
                list(r.globals.skills),
                strs(&g["skills"]),
                "{}: globals.skills",
                id
            );
            assert_eq!(
                list(r.globals.plugins),
                strs(&g["plugins"]),
                "{}: globals.plugins",
                id
            );
            assert_eq!(
                list(r.globals.rules),
                strs(&g["rules"]),
                "{}: globals.rules",
                id
            );
            assert_eq!(
                list(r.globals.memory),
                strs(&g["memory"]),
                "{}: globals.memory",
                id
            );
            assert_eq!(list(r.globals.mcp), strs(&g["mcp"]), "{}: globals.mcp", id);

            let p = &j["projects"];
            assert_eq!(
                r.projects.marker,
                p["marker"].as_str().unwrap_or_default(),
                "{}: marker",
                id
            );
            assert_eq!(
                list(r.projects.skills),
                strs(&p["skills"]),
                "{}: projects.skills",
                id
            );
            assert_eq!(
                list(r.projects.rules),
                strs(&p["rules"]),
                "{}: projects.rules",
                id
            );
            assert_eq!(
                list(r.projects.memory),
                strs(&p["memory"]),
                "{}: projects.memory",
                id
            );
            assert_eq!(
                list(r.projects.mcp),
                strs(&p["mcp"]),
                "{}: projects.mcp",
                id
            );
            match p["plugins"].as_array() {
                Some(_) => assert_eq!(
                    r.projects.plugins.map(list),
                    Some(strs(&p["plugins"])),
                    "{}: projects.plugins",
                    id
                ),
                None => assert!(
                    r.projects.plugins.is_none(),
                    "{}: snapshot declares no projects.plugins",
                    id
                ),
            }
        }
    }

    /// Every declared id resolves here, and an unknown id resolves to nothing instead
    /// of a guessed path.
    #[test]
    fn by_id_covers_every_declared_agent() {
        for j in declared_agents_json() {
            let id = j["id"].as_str().unwrap_or_default();
            assert!(by_id(id).is_some(), "by_id({}) must resolve", id);
        }
        assert!(by_id("").is_none());
        assert!(by_id("claude-cod").is_none());
    }

    /// Byte-exact: only the tilde is substituted, the rest of the token —
    /// including the slash right behind it — stays as written. path.resolve
    /// would normalise it away, so this asserts the raw string.
    #[test]
    fn resolve_home_substitutes_the_home_placeholder_byte_for_byte() {
        assert_eq!(
            declared_table_json()["fakeHome"].as_str().unwrap(),
            FAKE_HOME
        );
        let resolved = resolved_paths_json();
        for j in declared_agents_json() {
            let id = j["id"].as_str().unwrap_or_default();
            let want = resolved[id]["home"].as_str().unwrap_or_default();
            let got = resolve_home(by_id(id).unwrap().home, FAKE_HOME).unwrap_or_default();
            assert_eq!(got, want, "{}: resolve_home", id);
            assert!(
                want.starts_with(&format!("{}/", FAKE_HOME)),
                "{}: the separator after the tilde must survive (got {})",
                id,
                want
            );
        }
        // A bare tilde is the home itself; a token without one passes through.
        assert_eq!(resolve_home("~", FAKE_HOME).as_deref(), Some(FAKE_HOME));
        assert_eq!(resolve_home("", FAKE_HOME), None);
        assert_eq!(
            resolve_home("/opt/agent", FAKE_HOME).as_deref(),
            Some("/opt/agent")
        );
    }

    /// The blind spot asset parity cannot see: a template that resolves to the
    /// wrong directory is invisible while neither build has that directory.
    ///
    /// Driven from the Rust table (what this implementation walks) and checked
    /// against the snapshot in both directions — a template must exist on
    /// both sides, and the snapshot must not carry one we dropped.
    #[test]
    fn every_path_template_resolves_under_the_declared_home() {
        let resolved = resolved_paths_json();
        let mut checked = 0usize;
        for j in declared_agents_json() {
            let id = j["id"].as_str().unwrap_or_default();
            let agent = by_id(id).expect("id comes from the adapter table");
            let home = resolve_home(agent.home, FAKE_HOME).unwrap_or_default();
            let entry = &resolved[id];

            let global_groups: [(&str, &[&str]); 5] = [
                ("globals.skills", agent.globals.skills),
                ("globals.plugins", agent.globals.plugins),
                ("globals.rules", agent.globals.rules),
                ("globals.memory", agent.globals.memory),
                ("globals.mcp", agent.globals.mcp),
            ];
            let project_groups: [(&str, &[&str]); 5] = [
                ("projects.skills", agent.projects.skills),
                ("projects.rules", agent.projects.rules),
                ("projects.memory", agent.projects.memory),
                ("projects.mcp", agent.projects.mcp),
                ("projects.plugins", agent.projects.plugins.unwrap_or(&[])),
            ];

            let declared_global = entry["globalPaths"].as_object().unwrap();
            let declared_project = entry["projectPaths"].as_object().unwrap();
            assert_eq!(
                declared_global.len(),
                global_groups.iter().map(|(_, r)| r.len()).sum::<usize>(),
                "{}: globals template count",
                id
            );
            assert_eq!(
                declared_project.len(),
                project_groups.iter().map(|(_, r)| r.len()).sum::<usize>(),
                "{}: projects template count",
                id
            );

            for (bag, groups, base) in [
                (declared_global, &global_groups[..], home.as_str()),
                (declared_project, &project_groups[..], FAKE_PROJECT),
            ] {
                for (group, rels) in groups.iter() {
                    for rel in rels.iter() {
                        let key = format!("{}:{}", group, rel);
                        let want = bag
                            .get(&key)
                            .unwrap_or_else(|| {
                                panic!("{}: {} is missing from the snapshot", id, key)
                            })
                            .as_str()
                            .unwrap_or_default();
                        let got = join_native(Path::new(base), rel);
                        assert!(
                            same_path(want, &got.to_string_lossy()),
                            "{}: {} -> {} (js {})",
                            id,
                            key,
                            got.to_string_lossy(),
                            want
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert!(
            checked >= 150,
            "only {} templates were checked — the snapshot is incomplete",
            checked
        );
    }

    /// The shared directory is a constant rather than an adapter, so it drifts
    /// easily: assert both the token and its expansion.
    #[test]
    fn shared_skills_dir_is_the_declared_constant() {
        let table = declared_table_json();
        assert_eq!(
            SHARED_SKILLS_DIR,
            table["sharedSkillsDir"].as_str().unwrap()
        );
        let want = format!("{}/.agents/skills", FAKE_HOME);
        assert_eq!(
            resolve_home(SHARED_SKILLS_DIR, FAKE_HOME).as_deref(),
            Some(want.as_str())
        );
        assert_eq!(
            table["sharedSkills"].as_str().unwrap().replace('\\', "/"),
            want,
            "expansion of SHARED_SKILLS_DIR"
        );
    }
}
