//! `ak-scan` — headless scanner.
//!
//! Prints the exact `ak:scan` payload as JSON so a reference payload can be
//! diffed field by field. That is the guard against a silent data change:
//! every field of every asset is compared, not a summary count.
//!
//! Usage:
//!   ak-scan [user_home] [store_root]        scan payload
//!   ak-scan --read <asset_id> [home] [root] `read_asset` payload for one asset
//!   ak-scan --settings-get [root]           merged settings
//!   ak-scan --settings-save [root] <json>   apply a settings patch
//!   ak-scan --search-url <json_opts>        the exact registry URL
//!   ak-scan --search <json_opts>            live registry search payload
//!
//! `--read` runs the scan first (the command does the same on a cold cache), so
//! the drawer's response can be diffed against `read_asset`.
//!
//! The `--settings-*` modes exist for the same reason: they make the one write
//! path in this crate reachable without a GUI, so "does saving actually reach
//! disk on this machine" can be answered from a shell instead of by clicking.
//!
//! `--search-url` is the useful one: it lifts the URL builder out of
//! the process so the string can be diffed byte-for-byte, including the
//! space-as-`+` encoding and the clamp order.

use agentkeeper_core::{assets, discover, registry, scan, settings, validate};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Modes that never need a scan: keep them first so they stay fast.
    match args.get(1).map(|s| s.as_str()) {
        Some("--settings-get") => {
            let root = args.get(2).cloned().unwrap_or_else(default_store);
            print_json(&settings::read(&root));
            return;
        }
        Some("--settings-save") => {
            let root = args.get(2).cloned().unwrap_or_else(default_store);
            let patch = args.get(3).cloned().unwrap_or_else(|| "{}".to_string());
            match serde_json::from_str::<serde_json::Value>(&patch) {
                Ok(v) => {
                    let r = settings::save(&root, &v);
                    print_json(&r);
                }
                Err(e) => {
                    eprintln!("[ak-scan] patch is not valid JSON: {}", e);
                    std::process::exit(2);
                }
            }
            return;
        }
        Some("--validate") => {
            let raw = args.get(2).cloned().unwrap_or_else(|| "{}".to_string());
            let raw = match read_indirect(&raw) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[ak-scan] {}", e);
                    std::process::exit(2);
                }
            };
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(v) => {
                    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                    let payload = v.get("payload").cloned().unwrap_or(serde_json::json!({}));
                    print_json(&serde_json::Value::Array(validate::validate_by_kind(
                        kind, &payload,
                    )));
                }
                Err(e) => {
                    eprintln!("[ak-scan] case is not valid JSON: {}", e);
                    std::process::exit(2);
                }
            }
            return;
        }
        Some("--registry-add") => {
            let root = args.get(2).cloned().unwrap_or_else(default_store);
            let dir = args.get(3).cloned().unwrap_or_default();
            let name = args.get(4).cloned().unwrap_or_default();
            registry::ensure_loaded(&root);
            let cleaned = if name.is_empty() {
                None
            } else {
                Some(name.as_str())
            };
            print_json(&registry::add(&dir, cleaned));
            return;
        }
        Some("--registry-remove") => {
            let root = args.get(2).cloned().unwrap_or_else(default_store);
            let id = args.get(3).cloned().unwrap_or_default();
            registry::ensure_loaded(&root);
            print_json(&registry::remove(&id));
            return;
        }
        Some("--search-url") | Some("--search") => {
            let raw = args.get(2).cloned().unwrap_or_else(|| "{}".to_string());
            let opts = match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[ak-scan] options are not valid JSON: {}", e);
                    std::process::exit(2);
                }
            };
            if args[1] == "--search-url" {
                println!("{}", discover::search_url(&opts));
            } else {
                print_json(&discover::search_registry(&opts));
            }
            return;
        }
        _ => {}
    }

    let read_mode = args.get(1).map(|a| a == "--read").unwrap_or(false);
    let (home, store_root) = if read_mode {
        (
            args.get(3).cloned().unwrap_or_else(default_home),
            args.get(4).cloned().unwrap_or_else(default_store),
        )
    } else {
        (
            args.get(1).cloned().unwrap_or_else(default_home),
            args.get(2).cloned().unwrap_or_else(default_store),
        )
    };

    let projects = scan::read_projects(&store_root);
    let started = std::time::Instant::now();
    let (payload, timings) = scan::scan_payload(&home, &projects);
    let total = started.elapsed().as_millis();

    // Timings go to stderr so stdout stays a single clean JSON document.
    let detail: Vec<String> = timings
        .iter()
        .map(|(k, ms)| format!("{}={}ms", k, ms))
        .collect();
    eprintln!(
        "[ak-scan] {} total={}ms assets={}",
        detail.join(" "),
        total,
        payload
            .get("assets")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    );

    let out = if read_mode {
        let id = args.get(2).cloned().unwrap_or_default();
        assets::read_asset(&home, &store_root, &id)
    } else {
        payload
    };

    print_json(&out);
}

/// `@path` reads the JSON from a file instead of the argument list.
///
/// Windows caps a command line at ~32k characters, and a validator fixture
/// with a 200 KB body blows straight through that — the spawn fails with a
/// bare "argument list too long" that looks like a code bug.
fn read_indirect(arg: &str) -> Result<String, String> {
    match arg.strip_prefix('@') {
        None => Ok(arg.to_string()),
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read payload file {}: {}", path, e)),
    }
}

fn print_json(v: &serde_json::Value) {
    match serde_json::to_string(v) {
        Ok(s) => println!("{}", s),
        Err(e) => {
            eprintln!("[ak-scan] serialisation failed: {}", e);
            std::process::exit(1);
        }
    }
}

fn default_home() -> String {
    std::env::var("USERPROFILE").unwrap_or_default()
}

fn default_store() -> String {
    // Same root as `src/lib.rs` / `src/core/mcp.rs`: `<APPDATA>\AgentKeeper`
    // (productName, not the package name) plus the "agentkeeper" subdirectory.
    if let Ok(explicit) = std::env::var("AK_STORE_ROOT") {
        return explicit;
    }
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let base = std::env::var("APPDATA").unwrap_or_default();
    if base.is_empty() {
        return String::new();
    }
    format!("{0}{1}AgentKeeper{1}agentkeeper", base, sep)
}
