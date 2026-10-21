/**
 * Regenerate the parity snapshots in `tauri-shell/src-tauri/src/core/adapters.rs`.
 *
 * WHAT IT DOES
 *   Loads the JS adapter table (scripts/adapter-table.js) with a fake user home,
 *   evaluates every path template on the JS side, and freezes both the table
 *   and the resolved paths into two Rust string constants (`JS_TABLE_JSON`,
 *   `JS_RESOLVED_JSON`) at the end of that file. The Rust tests in `mod tests`
 *   then compare AGENTS / resolve_home / join_native against the frozen JS
 *   output.
 *
 * WHY IT EXISTS
 *   Asset parity ("both builds return the same assets") cannot see an adapter
 *   whose home directory does not exist on this machine: both builds agree
 *   about nothing. A snapshot taken from the JS source closes that blind spot —
 *   and it must be *generated*, never hand-copied, because a hand-copied
 *   snapshot silently freezes whatever the copier believed the JS said.
 *
 * WHEN IT MUST BE RE-RUN
 *   Any change to `scripts/adapter-table.js` — an added/removed/renamed agent, a
 *   changed `home` template, or any edited skills/rules/memory/mcp/plugins
 *   list. If you add an agent, also add it to `AGENTS` in adapters.rs; the
 *   generated tests will fail until you do, which is the point.
 *
 * HISTORY
 *   The table used to live in `core/adapters/index.js`, and the tilde expansion
 *   in `core/scanner.js:22`. Both are deleted with the pre-Tauri JS kernel; their
 *   semantics now live in `scripts/adapter-table.js`.
 *
 *   node scripts/gen-adapters-snapshots.js      (or: npm run tauri:adapters)
 */
'use strict';
const fs = require('fs');
const path = require('path');
const os = require('os');

const REPO = path.resolve(__dirname, '..');
const ADAPTERS_RS = path.join(REPO, 'tauri-shell/src-tauri/src/core/adapters.rs');
const FAKE_HOME = 'C:/ak-parity-home';
const FAKE_PROJECT = 'C:/ak-parity-project';

if (!fs.existsSync(ADAPTERS_RS)) {
  console.error('adapters.rs not found at ' + ADAPTERS_RS);
  process.exit(1);
}

os.homedir = () => FAKE_HOME; // replaces the real home for this process only
// Adapter table + tilde expansion now come from the single surviving copy:
// scripts/adapter-table.js (was core/adapters/index.js + core/scanner.js:22,
// both deleted with the pre-Tauri JS kernel).
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
  // globals resolve against the agent home (was core/scanner.js:690 — deleted,
  // semantics now documented in scripts/adapter-table.js).
  put(globalPaths, 'globals.skills', a.globals.skills, home);
  put(globalPaths, 'globals.plugins', a.globals.plugins, home);
  put(globalPaths, 'globals.rules', a.globals.rules, home);
  put(globalPaths, 'globals.memory', a.globals.memory, home);
  put(globalPaths, 'globals.mcp', a.globals.mcp, home);
  // projects resolve against the project dir (was core/scanner.js:716-722 —
  // deleted, semantics now documented in scripts/adapter-table.js).
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

// Pretty-print the table so a future diff is readable, but keep the resolved
// paths one agent per line.
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
    //! Snapshot tests against the JavaScript adapter table.
    //!
    //! Everything here is generated: run
    //!
    //!     npm run tauri:adapters      (scripts/gen-adapters-snapshots.js)
    //!
    //! after any change to scripts/adapter-table.js. The two JSON constants below
    //! are that script's output — the JS table loaded with os.homedir() set to
    //! FAKE_HOME, plus every path template resolved by the JS itself. Never
    //! edit them by hand: a hand-edited snapshot asserts whatever the editor
    //! believed, not what the JS says.
    use super::*;
    use crate::core::model::join_native;
    use serde_json::Value;
    use std::path::Path;

    /// Fake user home / project root the JS snapshot below was taken with.
    /// Nothing here touches the real machine — every assertion compares
    /// strings produced by the two adapter tables for the same fake input.
    const FAKE_HOME: &str = "${FAKE_HOME}";
    const FAKE_PROJECT: &str = "${FAKE_PROJECT}";

    /// scripts/adapter-table.js loaded with os.homedir() === FAKE_HOME, plus the
    /// family and notes fields.
    const JS_TABLE_JSON: &str = r##"${tableJson}"##;

    /// Every path template of every adapter, resolved on the JS side:
    /// path.resolve(home, rel) for globals and path.resolve(proj.dir, rel) for
    /// projects (was core/scanner.js:690 and :716-722 — deleted; semantics now
    /// in scripts/adapter-table.js), with home coming from resolveHome(), also
    /// in scripts/adapter-table.js (was core/scanner.js:22 — deleted).
    const JS_RESOLVED_JSON: &str = r##"${resolvedJson}"##;

    fn js_table() -> Value {
        serde_json::from_str(JS_TABLE_JSON).expect("JS adapter snapshot must be valid JSON")
    }

    fn js_resolved() -> Value {
        serde_json::from_str(JS_RESOLVED_JSON).expect("JS path snapshot must be valid JSON")
    }

    fn js_agents() -> Vec<Value> {
        match js_table().get("agents").and_then(|v| v.as_array()) {
            Some(a) => a.clone(),
            None => panic!("JS adapter snapshot has no agents array"),
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
    /// every separator to the native one, while this port keeps the slash that
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
    fn js_and_rust_tables_are_field_for_field_identical() {
        let js = js_agents();
        assert_eq!(
            js.len(),
            AGENTS.len(),
            "JS declares {} adapters, Rust declares {}",
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

            // notes is undefined on every adapter but two.
            let js_notes = j["notes"].as_str().map(|s| s.to_string());
            assert_eq!(r.notes.map(|s| s.to_string()), js_notes, "{}: notes", id);

            // family is a plain field in JS; this port derives it from the id.
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
                    "{}: JS declares no projects.plugins",
                    id
                ),
            }
        }
    }

    /// Every JS id resolves here, and an unknown id resolves to nothing instead
    /// of a guessed path.
    #[test]
    fn by_id_covers_every_js_agent() {
        for j in js_agents() {
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
    fn resolve_home_matches_the_js_substitution_byte_for_byte() {
        assert_eq!(js_table()["fakeHome"].as_str().unwrap(), FAKE_HOME);
        let resolved = js_resolved();
        for j in js_agents() {
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
        assert_eq!(resolve_home("/opt/agent", FAKE_HOME).as_deref(), Some("/opt/agent"));
    }

    /// The blind spot asset parity cannot see: a template that resolves to the
    /// wrong directory is invisible while neither build has that directory.
    ///
    /// Driven from the Rust table (what this port actually walks) and checked
    /// against the JS snapshot in both directions — a template must exist on
    /// both sides, and the snapshot must not carry one we dropped.
    #[test]
    fn every_path_template_resolves_to_the_js_directory() {
        let resolved = js_resolved();
        let mut checked = 0usize;
        for j in js_agents() {
            let id = j["id"].as_str().unwrap_or_default();
            let agent = by_id(id).expect("id comes from the JS table");
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

            let js_global = entry["globalPaths"].as_object().unwrap();
            let js_project = entry["projectPaths"].as_object().unwrap();
            assert_eq!(
                js_global.len(),
                global_groups.iter().map(|(_, r)| r.len()).sum::<usize>(),
                "{}: globals template count",
                id
            );
            assert_eq!(
                js_project.len(),
                project_groups.iter().map(|(_, r)| r.len()).sum::<usize>(),
                "{}: projects template count",
                id
            );

            for (bag, groups, base) in [
                (js_global, &global_groups[..], home.as_str()),
                (js_project, &project_groups[..], FAKE_PROJECT),
            ] {
                for (group, rels) in groups.iter() {
                    for rel in rels.iter() {
                        let key = format!("{}:{}", group, rel);
                        let want = bag
                            .get(&key)
                            .unwrap_or_else(|| panic!("{}: {} is missing from the JS snapshot", id, key))
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
    fn shared_skills_dir_matches_the_js_constant() {
        let table = js_table();
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
const marker = '\n#[cfg(test)]\nmod tests {';
if (src.includes(marker)) {
  src = src.slice(0, src.indexOf(marker));
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
    ' JS adapters and ' +
    templates +
    ' resolved path templates (' +
    src.length +
    ' chars).'
);
console.log('agents: ' + agents.map((a) => a.id).join(', '));
