//! 设置 + store 初始化 —— store 初始化与设置的读写。
//!
//! 设置是一次单文件 JSON 写入，具备与 store 其余部分相同的原子重命名保证，
//! 所以它们不走事务层。
//!
//! ## 语义
//!
//! * 只有 `defaults` 中存在的键才可以被写入，而且必须与默认值具有相同的
//!   `typeof`。一个手工编辑过的 `settings.json` 无法把字符串偷换成数字的位置
//!   （那样更新管理器就会每 `"abc"` 分钟安排一次检查）。
//! * 文件通过 `<path>.agentkeeper-tmp-<pid>-<ms>-<seq>` 写入后再重命名，重命名
//!   失败时退回直接写入（Windows：扫描器可能会短暂占住目标文件）。`seq` 那一半
//!   的存在是因为当两个任务在同一毫秒内写同一条路径时 `<pid>-<ms>` 并不唯一
//!   —— 见 [`atomic_write`]。
//! * 每次保存都会追加一条 `audit.jsonl` 记录。

use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::model::iso_from_millis;

/// `Store.defaultSettings()` —— 每个默认值的类型同时决定了哪些值可以覆盖它。
pub fn defaults() -> Value {
    json!({
        "lang": "auto",
        "theme": "system",
        "updateCheck": true,
        "updateCheckMinutes": 20,
        "updateBeta": false,
        "updateRepo": "ntc0728/agentkeeper",
        "updateApiBase": "",
        "updateProxy": "auto",
        "updateToken": "",
        "updateAssetKeyword": "AgentKeeper",
        "updateInstallSilent": true,
        "notifications": true,
        "showLocalStats": true
    })
}

fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    iso_from_millis(ms)
}

fn settings_path(root: &str) -> std::path::PathBuf {
    Path::new(root).join("settings.json")
}

fn meta_path(root: &str) -> std::path::PathBuf {
    Path::new(root).join("meta.json")
}

fn audit_path(root: &str) -> std::path::PathBuf {
    Path::new(root).join("audit.jsonl")
}

/// `typeof a === typeof b`，其中"对象"的含义更窄：数组和
/// 对象都是 `object`，null 也是 `object`（这就是文件里的一个 null 永远不会覆盖
/// 一个有类型的默认值的原因）。
fn same_type(a: &Value, b: &Value) -> bool {
    matches!(
        (a, b),
        (Value::String(_), Value::String(_))
            | (Value::Bool(_), Value::Bool(_))
            | (Value::Number(_), Value::Number(_))
            | (Value::Array(_), Value::Array(_))
            | (Value::Object(_), Value::Object(_))
    )
}

/// `Store.ensure()` —— 创建出应用其余部分假定存在的目录树。
///
/// 可以反复调用；它在每次启动时和每次设置写入时都会运行，这正是"首次启动"无需
/// 单独一步就能工作的原因。
///
/// 旧版本遗留的 `backups/` 在这里被**删除**。备份树不只是没用，它还会误导人：
/// 它是用户唯一会去寻找"撤销"的地方，而这个版本里没有任何东西能把它读回来。
/// 留着它意味着一个看起来像功能、实际不是的陈旧副本目录。
/// 删除范围限定在 `<root>/backups` —— 一个这个应用自己创建、且归它所有的目录
/// —— 并且是尽力而为的：一个被其它进程锁住的文件绝不该阻止应用启动。
pub fn ensure_store(root: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let audit = audit_path(root);
    if !audit.exists() {
        std::fs::write(&audit, "")?;
    }
    let meta = meta_path(root);
    if !meta.exists() {
        let body = serde_json::to_string_pretty(&json!({
            "version": 1,
            "createdAt": now_iso()
        }))
        .unwrap_or_default();
        std::fs::write(&meta, body)?;
    }
    remove_legacy_backups(root);
    Ok(())
}

/// 丢弃退役的 `<root>/backups` 目录树，忽略其中的一切失败。
///
/// 与 `ensure_store` 的函数体分开，好让"尽力而为、绝不阻塞启动"这条规则只陈述
/// 一次。Windows 共享或杀毒扫描器可能会占用这些文件中的一个；为一个没有任何
/// 东西会读的目录拒绝启动，比让它再多留一次运行更糟。
fn remove_legacy_backups(root: &str) {
    let legacy = Path::new(root).join("backups");
    // 用 `symlink_metadata`，这样被放在那条路径上的一个链接会被解除，而不是被
    // 跟随进它指向的任何地方。
    let Ok(meta) = std::fs::symlink_metadata(&legacy) else {
        return;
    };
    if meta.is_dir() {
        let _ = std::fs::remove_dir_all(&legacy);
    } else {
        let _ = std::fs::remove_file(&legacy);
    }
}

/// `readSettings()` —— 默认值，被文件有选择地覆盖。
pub fn read(root: &str) -> Value {
    let raw = std::fs::read(settings_path(root))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_else(|| json!({}));

    let d = defaults();
    let (Value::Object(d), Value::Object(r)) = (&d, &raw) else {
        return d.clone();
    };
    let mut out = Map::new();
    for (k, dv) in d {
        match r.get(k) {
            Some(v) if same_type(dv, v) => {
                out.insert(k.clone(), v.clone());
            }
            _ => {
                out.insert(k.clone(), dv.clone());
            }
        }
    }
    Value::Object(out)
}

/// `saveSettings(patch)` —— 过滤后的合并、原子写入、审计。
///
/// 返回与
/// 相同的 `{ ok, settings }` 信封，好让渲染层的
/// 成功路径完全一致。错误以 `{ ok: false, error }` 的形式浮现，而不是一次悄悄的
/// 成功：一次没有到达磁盘的设置变更绝不能看起来像是成功了。
pub fn save(root: &str, patch: &Value) -> Value {
    let current = read(root);
    let d = defaults();
    let mut next = current.as_object().cloned().unwrap_or_default();
    let mut applied: Vec<String> = Vec::new();

    if let (Value::Object(d), Value::Object(p)) = (&d, patch) {
        for (k, v) in p {
            // 未知键和类型不匹配会被忽略。
            let Some(dv) = d.get(k) else { continue };
            if !same_type(dv, v) {
                continue;
            }
            next.insert(k.clone(), v.clone());
            applied.push(k.clone());
        }
    }

    let next = Value::Object(next);
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(&next).unwrap_or_else(|_| "{}".to_string())
    );
    if let Err(e) = atomic_write(&settings_path(root), &body) {
        return json!({ "ok": false, "error": format!("settings.json write failed: {}", e) });
    }

    audit(
        root,
        &json!({
            "action": "settings-save",
            "status": "ok",
            "kind": "settings",
            "assetName": applied.join(",")
        }),
    );

    json!({ "ok": true, "settings": next })
}

/// `atomicWrite()` —— 同一目录下的临时文件，然后重命名。
///
/// 临时文件名必须**每次调用**唯一，而不是每毫秒唯一：两个任务在同一刻写*同一条*
/// 路径（一次双击保存，或者渲染层把同一个资产保存两次）否则会拿到同一个名字，
/// 而 `File::create` 会截断 —— 于是一个写入者会覆盖另一个写入者的临时文件，
/// 接着两者都会把混合物重命名到位，而这恰恰就是"临时文件 + 重命名"存在的目的
/// 要防止的事。pid 让一个残留的临时文件能追溯到那个没能清理掉它的进程，时间戳
/// 让它可读，而计数器才是真正让它唯一的东西。
fn temp_name(file_name: &str) -> String {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "{}.agentkeeper-tmp-{}-{}-{}",
        file_name,
        std::process::id(),
        stamp,
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(temp_name(&file_name));

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all().ok();
    }

    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Windows：病毒扫描器可能会短暂占住目标文件。
            // 丢弃临时文件并就地写入，仍然比让这次保存失败要好。
            //
            // 这是这个函数里唯一一处非原子的分支：在它执行期间，一个并发读者
            // 可能看到一个被截断的文件。这里并不通过重试来把它变得"安全" ——
            // 一个重试循环只会缩小窗口，而把结果称作原子的正是这个保证丢失的
            // 方式。有两个测试把这件事的两半分开验证。`tests/boundary.rs` 里
            // 针对同一路径的写入者断言读者绝不会观察到被撕裂的正文，所以如果
            // 这个分支在那条路径上曾经触发，套件就会失败，而不是让损坏悄悄
            // 发布出去。而
            // `a_target_held_without_delete_sharing_makes_atomic_write_fall_back_in_place`
            // 在没有 `FILE_SHARE_DELETE` 的情况下占住目标文件，而这正是逼出*这个*
            // 分支的原因，并断言保存仍然落地。第二个测试证明退回路径可行；它并
            // 不让那个窗口消失，这正是第一个测试仍然是关键保证的原因。
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(path, content)
        }
    }
}

/// `Store.audit()` —— 一条 JSONL 记录，`ts` 在最前。
pub fn audit(root: &str, rec: &Value) {
    let mut map = Map::new();
    map.insert("ts".to_string(), Value::String(now_iso()));
    if let Value::Object(r) = rec {
        for (k, v) in r {
            map.insert(k.clone(), v.clone());
        }
    }
    let line = format!("{}\n", Value::Object(map));
    let path = audit_path(root);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `Store.baselineDiff()` —— 自用户上次锚定以来有什么变化。
///
/// 返回健康卡片所读取的形状：`{ changed, added, gone, hasBaseline }`。
/// 还没有 `baseline.json` 时（在有人点击"建立基线"之前的常态）它报告
/// `hasBaseline: false`，而不是假装一切都是干净的 —— 而且更重要的是，不是
/// 失败：卡片在 `ok: false` 上会永远渲染"加载中……"。
pub fn baseline_diff(root: &str, assets: &[(String, String)]) -> Value {
    let base = read_baseline(root);
    let mut changed = Vec::new();
    let mut added = Vec::new();
    let mut gone = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (id, fp) in assets {
        seen.insert(id.as_str());
        match base.get(id) {
            None => added.push(Value::String(id.clone())),
            Some(prev) if prev != fp => changed.push(Value::String(id.clone())),
            Some(_) => {}
        }
    }
    for id in base.keys() {
        if !seen.contains(id.as_str()) {
            gone.push(Value::String(id.clone()));
        }
    }

    json!({
        "ok": true,
        "changed": changed,
        "added": added,
        "gone": gone,
        "hasBaseline": !base.is_empty()
    })
}

/// `Store.setBaseline()` —— 锚定当前的指纹。
///
/// 这是 store 根目录下的一个文件，具备与 settings.json 相同的原子重命名保证；
/// 它不触碰 agent 文件，所以不需要事务层。
pub fn set_baseline(root: &str, assets: &[(String, String)]) -> Value {
    let mut map = Map::new();
    for (id, fp) in assets {
        map.insert(id.clone(), Value::String(fp.clone()));
    }
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_else(|_| "{}".to_string())
    );
    if let Err(e) = atomic_write(&Path::new(root).join("baseline.json"), &body) {
        return json!({ "ok": false, "error": format!("baseline.json write failed: {}", e) });
    }
    audit(
        root,
        &json!({
            "kind": "settings",
            "action": "set-baseline",
            "status": "ok",
            "assetName": assets.len().to_string()
        }),
    );
    json!({ "ok": true, "count": assets.len() })
}

fn read_baseline(root: &str) -> Map<String, Value> {
    std::fs::read(Path::new(root).join("baseline.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/* ------------------------------------------------------------------ */
/* 渲染层崩溃通道（`ak:ui-error`）                                       */
/* ------------------------------------------------------------------ */

/// 保留多少条渲染层错误。刻意很小：这个文件是一条诊断尾巴，不是一个日志归档，
/// 而一个卡在错误循环里的渲染层每分钟能吐出几千行。
pub const UI_ERROR_LIMIT: usize = 200;

/// `message` / `stack` 的逐字段上限。一个带构建哈希和十几个帧的压缩栈稳稳在
/// 4 KB 以内；更长的就是一个 data URL 或一条失控的字符串，会把单条记录推进
/// "大文件"那一类。
pub const UI_ERROR_FIELD_MAX: usize = 4096;

/// 诊断信息与 `audit.jsonl` 并排放在 store 根目录，所以"打开 store 文件夹"
/// 就能看到它们，而不需要第二套路径约定。
pub const UI_ERROR_FILE: &str = "ui-errors.jsonl";

/// 把一个字符串截断到最多 `max` 个字节，且**在 UTF-8 字符边界上**。
///
/// `s[..n]` 在 `n` 切断一个字符时会 panic，而 `&s.as_bytes()[..n]` 返回的字节
/// 随后会被 `from_utf8` 拒绝 —— 一条在任意字节偏移处被截断的中文栈消息正是这种
/// 情形（每字符 3 字节，所以 4096 不是边界）。回退到最近的边界是唯一一种既让
/// 这一行仍能按 JSON 解析、又仍然可读的截断。
fn cut_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// 构造被追加的那条记录：五个有文档的字段（缺失时是空字符串，好让渲染层永远
/// 不必去测试 `null`），再加上每一个未知键，原样保留。
fn ui_error_record(payload: &Value) -> Value {
    let mut out = Map::new();
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    out.insert("kind".into(), Value::String(text("kind")));
    out.insert(
        "message".into(),
        Value::String(cut_utf8(&text("message"), UI_ERROR_FIELD_MAX)),
    );
    out.insert(
        "stack".into(),
        Value::String(cut_utf8(&text("stack"), UI_ERROR_FIELD_MAX)),
    );
    out.insert("view".into(), Value::String(text("view")));
    out.insert("at".into(), Value::String(text("at")));
    // 未知键逐字存活：渲染层会扩展这个负载，后端不必同步发版才能保留那些
    // 额外字段。
    if let Value::Object(map) = payload {
        for (k, v) in map {
            if !out.contains_key(k) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// `ak:ui-error` —— 追加一条渲染层错误，最多保留
/// [`UI_ERROR_LIMIT`] 行。
///
/// 采用"读取-裁剪-写入"而不是轮转日志：这个文件是有界的，所以整体都能放进内存，
/// 而另一种做法（追加 + 定期轮转）会留下一个其生命周期无人负责的第二文件。
///
/// 绝不 panic，也绝不向调用方返回 `Err` —— 一个报告崩溃的渲染层绝不该自己把
/// 应用搞崩。写入失败会以 `{"ok": false, "error": ...}` 的形式报告，并回显到
/// stderr，那正是打包构建的控制台会显示的地方。
///
/// **并发报告是安全的。** 读取-裁剪-写入在 [`UI_ERROR_LOCK`] 之下运行，所以
/// 同时抵达的两个渲染层错误会被串行化、两条行都存活；最后写入者再也无法保存它
/// 在另一个写入者写入之前读到的正文。在这里保证这一点代价很低（一个小文件，写入
/// 本身已被 [`UI_ERROR_LIMIT`] 限制），而不保证的代价很高：因竞态丢掉的那一行
/// 恰恰是解释用户面前那次崩溃的那一行。
pub fn ui_error(root: &str, payload: &Value) -> Value {
    match append_ui_error(root, payload) {
        Ok(count) => json!({ "ok": true, "count": count }),
        Err(e) => {
            // 同时也写 stderr：渲染层的 toast 是唯一的另一份拷贝，而它会随着
            // 报告错误的那个页面一起消失。
            eprintln!("[ui-error] could not append to {}: {}", UI_ERROR_FILE, e);
            json!({ "ok": false, "error": e.to_string() })
        }
    }
}

/// 串行化 [`append_ui_error`] 里的读取-裁剪-写入。
///
/// 为什么用一把锁而不是"追加、任由操作系统交错"：每一条报告都会重写整个有界的
/// 文件，所以两份重叠的报告会读到同一份旧正文，第二次写入就会悄悄丢掉第一行。
/// 追加能修好这一点，但会丢掉上限，而上限正是这个文件之所以是重写的原因。
///
/// 一把进程级的锁，而不是按 root 的映射：每个进程恰好只有一个 store 根目录，
/// 临界区就是一个小文件，而一个映射需要自己的生命周期（以及一条清理规则）却没有
/// 任何收益。
///
/// 用 `unwrap_or_else(|e| e.into_inner())` 而不是 `unwrap`：其它线程里的一次 panic
/// 绝不该永久毒化这把锁 —— 崩溃通道在第一次 panic 之后就沉默，恰恰就是它存在的
/// 目的所要报告的那种失败。
static UI_ERROR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn append_ui_error(root: &str, payload: &Value) -> std::io::Result<usize> {
    let dir = Path::new(root);
    // 缺失的 store 根目录在这里被创建而不是被假定存在：这条命令可能就是一个崩溃
    // 的渲染层在 `bootstrap` 之前调用的第一件事。
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(dir)?;
    }
    let path = dir.join(UI_ERROR_FILE);

    // 守卫在这里获取，围着读取*和*写入，因为这把锁必须覆盖读-改-写的两半：只守
    // 住写入仍然会让第二个报告者读到追加之前的正文。在这个单一入口处加锁
    // （而不是在 `ui_error` 里）还覆盖了将来任何直接追加的调用者，并把这条不变量
    // 放在会破坏它的代码旁边。不用 `try_lock`：在争用下跳过写入等于用丢掉一行
    // 换丢掉一份报告，更糟 —— 被拒绝的报告渲染层是不会重试的。
    let _guard = UI_ERROR_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();

    // 一行一个对象 —— 用 `to_string`，不要 pretty：一条 pretty 的记录会占掉好几
    // 行，同时破坏上限和任何基于行的读取器。
    lines.push(ui_error_record(payload).to_string());

    // 丢掉最旧的，绝不丢最新的：最近的那次错误才是解释用户面前那次崩溃的。
    if lines.len() > UI_ERROR_LIMIT {
        let excess = lines.len() - UI_ERROR_LIMIT;
        lines.drain(0..excess);
    }

    let mut body = lines.join("\n");
    body.push('\n');
    // 与 store 里其它每个文件相同的原子重命名保证：一个半写的诊断文件恰恰在
    // 最需要它的时候毫无价值。
    atomic_write(&path, &body)?;
    Ok(lines.len())
}

/// `Store.readAudit()` —— 最新的在前，可选过滤。
///
/// 返回一个**普通数组**，而不是信封：`ak:audit` 把它直接交给渲染层，而渲染层
/// 会做 `audit.filter(...)`。
pub fn read_audit(
    root: &str,
    limit: usize,
    kind: Option<&str>,
    agent_id: Option<&str>,
    project_id: Option<&str>,
) -> Vec<Value> {
    let text = std::fs::read_to_string(audit_path(root)).unwrap_or_default();
    let mut out = Vec::new();
    for line in text.split('\n').filter(|l| !l.trim().is_empty()).rev() {
        if out.len() >= limit {
            break;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let matches = |key: &str, want: Option<&str>| match want {
            Some(w) => rec.get(key).and_then(|v| v.as_str()) == Some(w),
            None => true,
        };
        if !matches("kind", kind)
            || !matches("agentId", agent_id)
            || !matches("projectId", project_id)
        {
            continue;
        }
        out.push(rec);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "ak-settings-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    /// 对同一个文件的两次写入绝不能共用一个临时名字，即使它们在同一毫秒内
    /// 开始：pid 和时钟无法区分它们，所以没有那个序列计数器时第二次
    /// `File::create` 会截断第一个写入者的临时文件，两者随后重命名那个混合物。
    ///
    /// 五十次调用而不是两次：单独一对可能碰巧跨过毫秒边界，而这正是这个 bug
    /// 会藏身的方式。五十个名字落在几微秒内则不可能。
    #[test]
    fn temp_file_names_are_unique_per_call() {
        let mut names = std::collections::HashSet::new();
        for _ in 0..50 {
            assert!(
                names.insert(temp_name("settings.json")),
                "a temp name repeated inside one burst — the next writer would truncate it"
            );
        }
        let sample = temp_name("settings.json");
        assert!(
            sample.starts_with("settings.json.agentkeeper-tmp-"),
            "the temp file must live next to its target: {}",
            sample
        );
        assert!(
            sample.contains(&std::process::id().to_string()),
            "the pid must stay in the name so a stale temp file is traceable: {}",
            sample
        );
    }

    #[test]
    fn read_returns_defaults_when_file_missing() {
        let root = temp_root("defaults");
        let s = read(&root);
        assert_eq!(s["lang"], json!("auto"));
        assert_eq!(s["updateCheckMinutes"], json!(20));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn read_ignores_wrong_types_and_unknown_keys() {
        let root = temp_root("types");
        std::fs::write(
            settings_path(&root),
            r#"{ "lang": 42, "updateCheckMinutes": "abc", "evil": true, "theme": "dark" }"#,
        )
        .unwrap();
        let s = read(&root);
        assert_eq!(
            s["lang"],
            json!("auto"),
            "number must not override a string"
        );
        assert_eq!(
            s["updateCheckMinutes"],
            json!(20),
            "string must not override a number"
        );
        assert_eq!(s["theme"], json!("dark"), "matching type is kept");
        assert!(
            s.get("evil").is_none(),
            "unknown keys never enter the merged view"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn save_writes_only_known_keys_with_trailing_newline() {
        let root = temp_root("save");
        let r = save(
            &root,
            &json!({ "theme": "dark", "evil": 1, "updateCheckMinutes": "x" }),
        );
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["settings"]["theme"], json!("dark"));
        assert_eq!(r["settings"]["updateCheckMinutes"], json!(20));

        let raw = std::fs::read_to_string(settings_path(&root)).unwrap();
        assert!(raw.ends_with("}\n"), "JS writes a trailing newline");
        assert!(!raw.contains("evil"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn save_is_a_merge_not_a_replace() {
        let root = temp_root("merge");
        save(&root, &json!({ "theme": "dark" }));
        save(&root, &json!({ "notifications": false }));
        let s = read(&root);
        assert_eq!(s["theme"], json!("dark"));
        assert_eq!(s["notifications"], json!(false));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn save_appends_an_audit_record() {
        let root = temp_root("audit");
        ensure_store(&root).unwrap();
        save(&root, &json!({ "lang": "zh" }));
        let log = std::fs::read_to_string(audit_path(&root)).unwrap();
        assert_eq!(log.lines().count(), 1);
        let rec: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(rec["kind"], json!("settings"));
        assert_eq!(rec["action"], json!("settings-save"));
        assert_eq!(rec["assetName"], json!("lang"));
        assert!(rec["ts"].as_str().unwrap().ends_with('Z'));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn read_audit_is_newest_first_and_filterable() {
        let root = temp_root("read-audit");
        ensure_store(&root).unwrap();
        let lines = [
            r#"{"ts":"1","kind":"scan","action":"scan"}"#,
            r#"{"ts":"2","kind":"settings","action":"settings-save"}"#,
            r#"{"ts":"3","kind":"mcp","action":"upsert","agentId":"a1"}"#,
        ];
        std::fs::write(audit_path(&root), lines.join("\n") + "\n").unwrap();

        let all = read_audit(&root, 10, None, None, None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0]["ts"], json!("3"), "newest first");
        assert_eq!(all[2]["ts"], json!("1"));

        assert_eq!(read_audit(&root, 10, Some("settings"), None, None).len(), 1);
        assert_eq!(read_audit(&root, 10, None, Some("a1"), None).len(), 1);
        assert_eq!(read_audit(&root, 10, None, Some("nope"), None).len(), 0);
        assert_eq!(
            read_audit(&root, 2, None, None, None).len(),
            2,
            "limit applies"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn baseline_round_trip_reports_added_changed_gone() {
        let root = temp_root("baseline");
        ensure_store(&root).unwrap();
        let first = vec![
            ("a".to_string(), "fp1".to_string()),
            ("b".to_string(), "fp2".to_string()),
        ];

        // 还没有基线：hasBaseline 为 false，一切看起来都像是"新增"。
        let d = baseline_diff(&root, &first);
        assert_eq!(d["hasBaseline"], json!(false));
        assert_eq!(d["added"].as_array().unwrap().len(), 2);

        let r = set_baseline(&root, &first);
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["count"], json!(2));

        let d = baseline_diff(&root, &first);
        assert_eq!(d["hasBaseline"], json!(true));
        assert_eq!(d["changed"].as_array().unwrap().len(), 0);

        let second = vec![
            ("a".to_string(), "fp1-CHANGED".to_string()),
            ("c".to_string(), "fp3".to_string()),
        ];
        let d = baseline_diff(&root, &second);
        assert_eq!(d["changed"], json!(["a"]));
        assert_eq!(d["added"], json!(["c"]));
        assert_eq!(d["gone"], json!(["b"]));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn atomic_write_replaces_existing_content() {
        let root = temp_root("atomic");
        let p = Path::new(&root).join("x.json");
        atomic_write(&p, "one").unwrap();
        atomic_write(&p, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        // 没有临时文件残留。
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("agentkeeper-tmp"))
            .collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ensure_store_creates_the_tree_once() {
        let root = temp_root("tree");
        ensure_store(&root).unwrap();
        ensure_store(&root).unwrap();
        assert!(audit_path(&root).is_file());
        assert!(
            !Path::new(&root).join("backups").exists(),
            "no backup tree is created any more"
        );
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(meta_path(&root)).unwrap()).unwrap();
        assert_eq!(meta["version"], json!(1));
        std::fs::remove_dir_all(&root).ok();
    }

    /// 一个由旧版本写出的配置档仍然有一个装满陈旧副本的 `backups/` 目录。它会在
    /// 下一次 `ensure_store` 时被删除：这个版本里没有任何东西能把它读回来，而一个
    /// 看起来像"撤销"却什么都不做的目录比没有目录更糟。删除范围限定在
    /// `<root>/backups` —— 应用自己的路径 —— 绝不是那些副本来源的 agent 目录。
    #[test]
    fn ensure_store_removes_a_legacy_backups_directory() {
        let root = temp_root("legacy-backups");
        let legacy = Path::new(&root).join("backups").join("tx-old");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("tx.json"), r#"{"txId":"tx-old"}"#).unwrap();

        ensure_store(&root).unwrap();
        assert!(
            !Path::new(&root).join("backups").exists(),
            "the retired backup tree must be gone after ensure_store"
        );
        // 应用真正读取的一切仍然都在。
        assert!(audit_path(&root).is_file());
        assert!(meta_path(&root).is_file());
        std::fs::remove_dir_all(&root).ok();
    }

    /// 第二次启动绝不能把它重建出来，也绝不能把它的缺失当成一个错误。
    #[test]
    fn ensure_store_is_idempotent_after_the_backup_tree_is_gone() {
        let root = temp_root("legacy-backups-twice");
        std::fs::create_dir_all(Path::new(&root).join("backups")).unwrap();
        ensure_store(&root).unwrap();
        ensure_store(&root).unwrap();
        assert!(!Path::new(&root).join("backups").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
