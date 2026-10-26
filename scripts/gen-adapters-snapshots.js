/**
 * 重新生成 `tauri-shell/core/src/adapters.rs` 里的对齐快照。
 *
 * 它做什么
 *   用一个假的用户主目录加载声明的适配器表（scripts/adapter-table.js），
 *   求值每一条路径模板，然后把表本身和解析出来的路径一起冻结成该文件末尾的
 *   两个 Rust 字符串常量（`DECLARED_TABLE_JSON`、
 *   `RESOLVED_PATHS_JSON`）。`mod tests` 里的 Rust 测试随后拿
 *   AGENTS / resolve_home / join_native 与这份冻结下来的 JS
 *   输出逐一比对。
 *
 * 它为什么存在
 *   资产对齐（两个构建返回同样的资产）看不见那些主目录在本机上并不存在的
 *   适配器：两个构建什么都对不上，什么都没说到。从声明源头取来的快照正好补上
 *   这个盲区——而且它必须是*生成*出来的，绝不能手抄，因为手抄的
 *   快照会悄无声息地冻结下抄写者以为的源头内容。
 *
 * 什么时候必须重跑
 *   `scripts/adapter-table.js` 的任何改动——新增/删除/重命名 agent、
 *   改过 `home` 模板，或者 skills/rules/memory/mcp/plugins
 *   列表被编辑过。如果你新增一个 agent，记得同时把它加到 adapters.rs 的
 *   `AGENTS` 里；生成的测试会在你这么做之前一直失败，而这正是本意。
 *
 *   node scripts/gen-adapters-snapshots.js      (or: npm run tauri:adapters)
 */
'use strict';
const fs = require('fs');
const path = require('path');
const os = require('os');

const REPO = path.resolve(__dirname, '..');
const ADAPTERS_RS = path.join(REPO, 'tauri-shell', 'core', 'src', 'adapters.rs');
const FAKE_HOME = 'C:/ak-parity-home';
const FAKE_PROJECT = 'C:/ak-parity-project';

if (!fs.existsSync(ADAPTERS_RS)) {
  console.error('adapters.rs not found at ' + ADAPTERS_RS);
  process.exit(1);
}

os.homedir = () => FAKE_HOME; // 只在这个进程里替换真实主目录
// 适配器表和波浪号展开都取自 scripts/adapter-table.js 里的那一份唯一副本。
const { AGENTS, SHARED_SKILLS_DIR, resolveHome } = require(path.join(REPO, 'scripts/adapter-table'));

const agents = [];
const resolved = {};
for (const a of AGENTS) {
  const home = resolveHome(a.home);
  const globalPaths = {};
  const projectPaths = {};
  const put = (bag, group, list, base) => {
    for (const rel of list || []) bag[group + ':' + rel] = path.resolve(base, rel);
  };
  // globals 相对 agent 主目录解析。
  put(globalPaths, 'globals.skills', a.globals.skills, home);
  put(globalPaths, 'globals.plugins', a.globals.plugins, home);
  put(globalPaths, 'globals.rules', a.globals.rules, home);
  put(globalPaths, 'globals.memory', a.globals.memory, home);
  put(globalPaths, 'globals.mcp', a.globals.mcp, home);
  // projects 相对项目目录解析。
  put(projectPaths, 'projects.skills', a.projects.skills, FAKE_PROJECT);
  put(projectPaths, 'projects.rules', a.projects.rules, FAKE_PROJECT);
  put(projectPaths, 'projects.memory', a.projects.memory, FAKE_PROJECT);
  put(projectPaths, 'projects.mcp', a.projects.mcp, FAKE_PROJECT);
  put(projectPaths, 'projects.plugins', a.projects.plugins, FAKE_PROJECT);

  agents.push({
    id: a.id,
    label: a.label,
    vendor: a.vendor,
    family: a.family,
    accent: a.accent,
    home: a.home,
    notes: a.notes || null,
    globals: {
      skills: a.globals.skills || [],
      plugins: a.globals.plugins || [],
      rules: a.globals.rules || [],
      memory: a.globals.memory || [],
      mcp: a.globals.mcp || []
    },
    projects: {
      marker: a.projects.marker,
      skills: a.projects.skills || [],
      rules: a.projects.rules || [],
      memory: a.projects.memory || [],
      mcp: a.projects.mcp || [],
      plugins: a.projects.plugins || null
    }
  });
  resolved[a.id] = { home, globalPaths, projectPaths };
}

const snapshot = {
  fakeHome: FAKE_HOME,
  fakeProject: FAKE_PROJECT,
  sharedSkillsDir: SHARED_SKILLS_DIR,
  sharedSkills: path.resolve(resolveHome(SHARED_SKILLS_DIR)),
  agents
};

// 把表漂亮地打印出来，好让以后的 diff 读得懂，但解析后的路径仍保持
// 一个 agent 一行。
const tableJson = JSON.stringify(snapshot, null, 1);
const resolvedJson =
  '{\n' +
  Object.entries(resolved)
    .map(([id, v]) => '  ' + JSON.stringify(id) + ': ' + JSON.stringify(v))
    .join(',\n') +
  '\n}';

const rust = String.raw`
#[cfg(test)]
mod tests {
    //! 针对声明式适配器表的快照测试。
    //!
    //! 这里的一切都是生成的：请在
    //!
    //!     npm run tauri:adapters      (scripts/gen-adapters-snapshots.js)
    //!
    //! 之后、即每次改动 scripts/adapter-table.js 之后重跑。下面那两个 JSON
    //! 常量就是那个脚本的输出——把 os.homedir() 设成 FAKE_HOME 求值后的
    //! 适配器表，加上每一条相对它解析出来的路径模板。绝不要
    //! 手工编辑它们：手工改过的快照断言的是编辑者以为的内容，
    //! 而不是表里真正写的内容。
    use super::*;
    use crate::model::join_native;
    use serde_json::Value;
    use std::path::Path;

    /// 下面这份快照拍摄时所用的假用户主目录 / 项目根。
    /// 这里没有任何东西会碰到真实机器——每一条断言比较的都是
    /// 两份适配器表针对同一份假输入产生的字符串。
    const FAKE_HOME: &str = "${FAKE_HOME}";
    const FAKE_PROJECT: &str = "${FAKE_PROJECT}";

    /// 以 os.homedir() === FAKE_HOME 加载的 scripts/adapter-table.js，
    /// 外加 family 与 notes 字段。
    const DECLARED_TABLE_JSON: &str = r##"${tableJson}"##;

    /// 每个适配器的每一条路径模板，在参考侧解析后的结果：
    /// globals 用 path.resolve(home, rel)，projects 用 path.resolve(proj.dir, rel)，
    /// 其中 home 来自 scripts/adapter-table.js 里的 resolveHome()。
    const RESOLVED_PATHS_JSON: &str = r##"${resolvedJson}"##;

    fn declared_table_json() -> Value {
        serde_json::from_str(DECLARED_TABLE_JSON).expect("adapter snapshot must be valid JSON")
    }

    fn resolved_paths_json() -> Value {
        serde_json::from_str(RESOLVED_PATHS_JSON).expect("JS path snapshot must be valid JSON")
    }

    fn declared_agents_json() -> Vec<Value> {
        match declared_table_json().get("agents").and_then(|v| v.as_array()) {
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

    /// 按文件系统的方式比较两条路径。path.resolve 会把
    /// 每个分隔符改写成本机形式，而这个实现保留了 resolve_home
    /// 在波浪号之后保留的那个斜杠，所以两个原始字符串可能只在
    /// 分隔符上不同。必须一致的是它们指向的目录。
    fn same_path(js: &str, rust: &str) -> bool {
        fn norm(s: &str) -> String {
            s.replace('\\', "/")
        }
        norm(js) == norm(rust)
    }

    /// 17 个适配器，声明顺序保持不变，每个字段都完全一致。
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
            assert_eq!(r.label, j["label"].as_str().unwrap_or_default(), "{}: label", id);
            assert_eq!(r.vendor, j["vendor"].as_str().unwrap_or_default(), "{}: vendor", id);
            assert_eq!(r.accent, j["accent"].as_str().unwrap_or_default(), "{}: accent", id);
            assert_eq!(r.home, j["home"].as_str().unwrap_or_default(), "{}: home", id);

            // notes 在除两个之外的所有适配器上都是 undefined。
            let declared_notes = j["notes"].as_str().map(|s| s.to_string());
            assert_eq!(r.notes.map(|s| s.to_string()), declared_notes, "{}: notes", id);

            // family 是个普通字段；这个实现从 id 推导它。
            assert_eq!(
                family(r.id),
                j["family"].as_str().unwrap_or_default(),
                "{}: family",
                id
            );

            let g = &j["globals"];
            assert_eq!(list(r.globals.skills), strs(&g["skills"]), "{}: globals.skills", id);
            assert_eq!(list(r.globals.plugins), strs(&g["plugins"]), "{}: globals.plugins", id);
            assert_eq!(list(r.globals.rules), strs(&g["rules"]), "{}: globals.rules", id);
            assert_eq!(list(r.globals.memory), strs(&g["memory"]), "{}: globals.memory", id);
            assert_eq!(list(r.globals.mcp), strs(&g["mcp"]), "{}: globals.mcp", id);

            let p = &j["projects"];
            assert_eq!(r.projects.marker, p["marker"].as_str().unwrap_or_default(), "{}: marker", id);
            assert_eq!(list(r.projects.skills), strs(&p["skills"]), "{}: projects.skills", id);
            assert_eq!(list(r.projects.rules), strs(&p["rules"]), "{}: projects.rules", id);
            assert_eq!(list(r.projects.memory), strs(&p["memory"]), "{}: projects.memory", id);
            assert_eq!(list(r.projects.mcp), strs(&p["mcp"]), "{}: projects.mcp", id);
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

    /// 每个声明过的 id 在这里都能解析出来，而未知 id 解析不到任何东西，
    /// 而不是猜出一条路径。
    #[test]
    fn by_id_covers_every_declared_agent() {
        for j in declared_agents_json() {
            let id = j["id"].as_str().unwrap_or_default();
            assert!(by_id(id).is_some(), "by_id({}) must resolve", id);
        }
        assert!(by_id("").is_none());
        assert!(by_id("claude-cod").is_none());
    }

    /// 字节级精确：只有波浪号被替换，token 的其余部分——
    /// 包括紧跟在它后面的那个斜杠——都保持原样。path.resolve
    /// 会把它归一化掉，所以这里断言的是原始字符串。
    #[test]
    fn resolve_home_substitutes_the_home_placeholder_byte_for_byte() {
        assert_eq!(declared_table_json()["fakeHome"].as_str().unwrap(), FAKE_HOME);
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
        // 光秃秃的波浪号就是主目录本身；不含波浪号的 token 原样透传。
        assert_eq!(resolve_home("~", FAKE_HOME).as_deref(), Some(FAKE_HOME));
        assert_eq!(resolve_home("", FAKE_HOME), None);
        assert_eq!(resolve_home("/opt/agent", FAKE_HOME).as_deref(), Some("/opt/agent"));
    }

    /// 资产对齐看不见的那个盲区：一个解析到错误目录的模板，
    /// 在两个构建都没有那个目录时是看不见的。
    ///
    /// 由 Rust 表驱动（也就是这个实现遍历的那份），并与快照
    /// 双向核对——一个模板必须在两边都存在，
    /// 而且快照里不能带着一条我们丢掉的模板。
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
                            .unwrap_or_else(|| panic!("{}: {} is missing from the snapshot", id, key))
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

    /// 共享目录是一个常量而不是一个适配器，所以它很容易漂移：
    /// token 和它的展开结果都要断言。
    #[test]
    fn shared_skills_dir_is_the_declared_constant() {
        let table = declared_table_json();
        assert_eq!(SHARED_SKILLS_DIR, table["sharedSkillsDir"].as_str().unwrap());
        let want = format!("{}/.agents/skills", FAKE_HOME);
        assert_eq!(resolve_home(SHARED_SKILLS_DIR, FAKE_HOME).as_deref(), Some(want.as_str()));
        assert_eq!(
            table["sharedSkills"].as_str().unwrap().replace('\\', "/"),
            want,
            "expansion of SHARED_SKILLS_DIR"
        );
    }
}
`;

let src = fs.readFileSync(ADAPTERS_RS, 'utf8');
// 在第一个生成块处截断，不管行尾是哪种：
// 文件可能是 CRLF，而一个用 LF 拼写的标记就匹配不上它——
// 于是 `indexOf` 返回 -1，`slice(0, -1)` 保留了全部内容，
// 生成器就会在每次运行时都追加一份该块的副本。
const marker = /\r?\n#\[cfg\(test\)\]\r?\nmod tests \{/;
const m = marker.exec(src);
if (m) {
  src = src.slice(0, m.index);
}
src = src.replace(/\s+$/, '') + '\n' + rust;
fs.writeFileSync(ADAPTERS_RS, src);

const templates = Object.values(resolved).reduce(
  (n, v) => n + Object.keys(v.globalPaths).length + Object.keys(v.projectPaths).length,
  0
);
console.log(
  'adapters.rs: froze ' +
    agents.length +
    ' adapters and ' +
    templates +
    ' resolved path templates (' +
    src.length +
    ' chars).'
);
console.log('agents: ' + agents.map((a) => a.id).join(', '));
