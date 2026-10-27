//! `ak-scan`——无界面扫描器。
//!
//! 它把 `ak:scan` 载荷原样打成 JSON，这样参考载荷可以逐字段 diff。这是防数据悄悄
//! 变化的闸门：比较的是每一条资产的每一个字段，而不是一个汇总计数。
//!
//! 用法：
//!   ak-scan [user_home] [store_root]        扫描载荷
//!   ak-scan --read <asset_id> [home] [root] 单条资产的 `read_asset` 载荷
//!   ak-scan --settings-get [root]           合并后的设置
//!   ak-scan --settings-save [root] <json>   应用一次设置补丁
//!   ak-scan --search-url <json_opts>        精确的 registry URL
//!   ak-scan --search <json_opts>            实时的 registry 搜索载荷
//!
//! `--read` 会先跑一次扫描（冷缓存时命令本身也这么做），这样抽屉的返回结果就能
//! 和 `read_asset` 对比。
//!
//! `--settings-*` 这几种模式存在的理由相同：它们让本 crate 里唯一的那条写入路径
//! 脱离 GUI 也能触达，于是"在这台机器上保存到底有没有落盘"可以从 shell 里回答，
//! 而不用靠点鼠标。
//!
//! 最有用的是 `--search-url`：它把 URL 拼装器从进程里提出来，于是那串字符可以
//! 逐字节 diff，包括空格编成 `+` 以及各参数取整（clamp）的先后顺序。

use agentkeeper_core::{assets, discover, registry, scan, settings, validate};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // 这些模式都不需要扫描：放在最前面，保证它们够快。
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

    // 计时信息走 stderr，好让 stdout 保持为一份干净的 JSON 文档。
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

/// `@path` 形式从文件读 JSON，而不是从参数列表。
///
/// Windows 命令行上限约 32k 字符，而一份带 200 KB body 的校验器用例直接把它撑爆
/// ——spawn 只会报一句干巴巴的 "argument list too long"，看起来像代码 bug。
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
    // 与 `src/lib.rs` / `src/core/mcp.rs` 同一个根：`<APPDATA>\AgentKeeper`
    // （是 productName，不是包名）再加一级 "agentkeeper" 子目录。
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
