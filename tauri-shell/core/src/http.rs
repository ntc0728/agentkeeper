//! 供那两个需要 HTTP 的地方使用的最小阻塞式 HTTP：注册表搜索和更新器。
//!
//! 放在一个文件里，是为了只有一套超时策略、一个 user-agent，以及传输层必须搬家时
//! （一个代理设置，或者复用 WebView2 的网络栈）唯一要改的地方。`core/` 必须不
//! 依赖异步运行时，所以这里是同步的。
//!
//! ## 出网策略
//!
//! `resolve_routes` 复现参考实现的路由：`auto` 先试直连、然后试本机 Clash
//! mixed 端口，`direct` 是一条路由，而一个被钉住的代理 URL 就是恰好一条路由
//! （被钉住的路由绝不能悄悄回退）。`https://` 代理 URL 和百分号编码的代理凭据
//! 不被 `ureq` 支持；那些设置会退化成 `auto`，与一个无法解析的值完全一样。
//! 两处缺口都记录在下面的限制里。

use serde_json::Value;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

#[derive(Debug)]
pub enum FetchError {
    /// 非 2xx 的响应。
    Http(u16),
    /// 响应到达之前截止时间就过了。
    Timeout,
    /// DNS 失败、连接被拒、TLS 错误、正文格式错误……
    Transport(String),
}

impl FetchError {
    pub fn is_timeout(&self) -> bool {
        matches!(self, FetchError::Timeout)
    }

    /// 参考实现的 `searchRegistry` 把中止映射为 `'registry timeout'`，把其它任何
    /// 失败映射为 `` `registry unreachable: ${e.message}` ``。消息文本天然因运行时
    /// 而异；UI 依据的是*分类*，而这一点被保留了下来。
    pub fn describe(&self) -> String {
        match self {
            FetchError::Http(code) => format!("http {}", code),
            FetchError::Timeout => "timeout".to_string(),
            FetchError::Transport(msg) => msg.clone(),
        }
    }
}

/// `ureq` 没有公开的超时判定谓词，所以这里沿着错误链去找套接字截止时间产生的
/// `io::ErrorKind`，只有找不到时才退回消息文本。
///
/// 单靠文本检查是依赖语言环境的：在非英文的 Windows 上，读取超时会以本地化的
/// OS 消息抵达，结尾是 `(os error 10060)` —— 没有 "timed out"，也没有 "timeout"
/// —— 于是注册表查询报告的是*registry unreachable: …* 而不是 *registry timeout*。
/// 由 `tests/boundary.rs::a_silent_peer_times_out_the_registry_query` 发现；正是
/// 这个 kind 检查让分类与显示语言无关。
pub(crate) fn is_timeout_error(error: &ureq::Error) -> bool {
    let ureq::Error::Transport(transport) = error else {
        return false;
    };
    let mut source = std::error::Error::source(transport);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                return true;
            }
        }
        source = err.source();
    }
    let lower = transport.to_string().to_lowercase();
    lower.contains("timed out") || lower.contains("timeout")
}

fn is_timeout(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::Status(_, _) => false,
        ureq::Error::Transport(_) => is_timeout_error(e),
    }
}

/// 以一个覆盖连接、TLS 和正文的硬性截止时间 GET 一个 JSON 文档。
pub fn get_json(url: &str, timeout_ms: u64) -> Result<Value, FetchError> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(timeout_ms.max(1)))
        // 刻意不跟随重定向：如果客户端悄悄跟随一个 302 去了别处，那么对*原始*主机
        // 的允许列表检查就毫无价值。3xx 会以 `FetchError::Http(3xx)` 的形式浮现。
        .redirects(0)
        .user_agent(concat!("AgentKeeper/", env!("CARGO_PKG_VERSION")))
        .build();

    let resp = agent
        .get(url)
        .set("accept", "application/json")
        .call()
        .map_err(|e| {
            if is_timeout(&e) {
                FetchError::Timeout
            } else {
                match e {
                    ureq::Error::Status(code, _) => FetchError::Http(code),
                    other => FetchError::Transport(other.to_string()),
                }
            }
        })?;

    let text = resp
        .into_string()
        .map_err(|e| FetchError::Transport(e.to_string()))?;
    serde_json::from_str::<Value>(&text)
        .map_err(|e| FetchError::Transport(format!("bad json: {}", e)))
}

/* ------------------------------------------------------------------ */
/* 共用的请求管道                                                        */
/* ------------------------------------------------------------------ */

/// 更新器发送的 UA（`USER_AGENT`）。不是上面那个带版本号的：GitHub 从两个构建
/// 看到的是同一个字符串。
pub(crate) const UPDATER_USER_AGENT: &str = "AgentKeeper-updater";

/// 参考实现的 `depth < 5` 规则：跟随五次重定向，第六个响应连同它的 3xx 状态
/// 一起返回给调用方。
const MAX_REDIRECTS: u32 = 5;

fn is_redirect(status: u16) -> bool {
    // 与下方的 `REDIRECTS` 表一致。
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn parse_content_length(raw: Option<&str>) -> Option<u64> {
    // `Number(headers['content-length']) || 0`：无法解析或为空的
    // 头部表现得就像 0，也就是"没有声明长度"。
    raw.and_then(|value| value.trim().parse::<u64>().ok())
}

/// 把一个 `Location` 头相对于请求 URL 解析出来。
///
/// 参考实现用 `new URL(location, url).href`。这覆盖了绝对 URL、协议相对 URL、
/// 绝对路径和路径相对 URL —— GitHub 及其 CDN 发送的就是这些。点分段*不会*被
/// 折叠，也没有百分号编码的规范化。
fn resolve_redirect(base: &str, location: &str) -> Option<String> {
    let location = location.trim();
    if location.is_empty() {
        return None;
    }
    if location.starts_with("//") {
        let scheme = base.split(':').next()?;
        return Some(format!("{}:{}", scheme, location));
    }
    // "https:"、"http:" 或任何其它协议：已经是绝对地址了。
    if let Some(colon) = location.find(':') {
        let scheme = &location[..colon];
        let is_scheme = !scheme.is_empty()
            && scheme.as_bytes()[0].is_ascii_alphabetic()
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
        if is_scheme {
            return Some(location.to_string());
        }
    }

    let (scheme, rest) = base.split_once("://")?;
    let authority_end = rest
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let path_and_more = &rest[authority_end..];

    if location.starts_with('/') {
        return Some(format!("{}://{}{}", scheme, authority, location));
    }
    if location.starts_with('?') {
        let path = path_and_more.split(['?', '#']).next().unwrap_or("/");
        return Some(format!("{}://{}{}{}", scheme, authority, path, location));
    }
    if location.starts_with('#') {
        return Some(format!(
            "{}://{}{}{}",
            scheme, authority, path_and_more, location
        ));
    }

    let path = path_and_more.split(['?', '#']).next().unwrap_or("/");
    let dir = match path.rfind('/') {
        Some(index) => &path[..=index],
        None => "/",
    };
    Some(format!("{}://{}{}{}", scheme, authority, dir, location))
}

/// `describeError` 的措辞，用于 ureq 传输层失败。
/// 这里的契约就是消息文本：设置面板会逐字渲染它。
pub(crate) fn describe_ureq_error(error: &ureq::Error) -> String {
    let ureq::Error::Transport(transport) = error else {
        return error.to_string();
    };
    let message = transport.to_string();
    let lower = message.to_lowercase();

    if matches!(transport.kind(), ureq::ErrorKind::Dns)
        || lower.contains("dns")
        || lower.contains("no such host")
        || lower.contains("name or service not known")
    {
        return "DNS lookup failed".to_string();
    }
    if is_timeout_error(error) {
        return "connection timed out".to_string();
    }
    if lower.contains("refused") {
        return "connection refused (is the proxy running?)".to_string();
    }
    if lower.contains("reset by peer") || lower.contains("connection reset") {
        return "connection was reset".to_string();
    }
    if lower.contains("certificate")
        || lower.contains("tls")
        || lower.contains("handshake")
        || lower.contains("ssl")
    {
        // 运行时错误码（`TLS 失败（EPROTO）`）由渲染层给出；ureq 没有码，所以
        // 细节被放进括号里。
        return format!("TLS failed ({})", message);
    }
    message
}

fn describe_io_error(error: &std::io::Error) -> String {
    // 与 `describeError` 相同的措辞；UI 会逐字渲染它们。
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            "connection timed out".to_string()
        }
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
            "connection was reset".to_string()
        }
        std::io::ErrorKind::ConnectionRefused => {
            "connection refused (is the proxy running?)".to_string()
        }
        _ => {
            let text = error.to_string();
            if text.to_lowercase().contains("timed out") {
                "connection timed out".to_string()
            } else {
                text
            }
        }
    }
}

/// 一条出网候选（`resolveCandidates`）。
pub(crate) struct Route {
    /// 显示在尝试日志里，例如 `直连` 或 `127.0.0.1:7897`。目前还没有东西读它：
    /// manager 没有 `onRouteFailure` 的日志钩子。
    #[allow(dead_code)]
    pub label: String,
    pub proxy: Option<ureq::Proxy>,
}

/// `CLASH_MIXED`：默认的本机 mixed 端口。
const CLASH_HOST: &str = "127.0.0.1";
const CLASH_PORT: u16 = 7897;

fn direct_route() -> Route {
    Route {
        label: "direct connection".to_string(),
        proxy: None,
    }
}

fn clash_route() -> Route {
    Route {
        label: format!("{}:{}", CLASH_HOST, CLASH_PORT),
        proxy: ureq::Proxy::new(format!("http://{}:{}", CLASH_HOST, CLASH_PORT)).ok(),
    }
}

/// `parseProxy`。`None` 意味着"不是一个可用的代理
/// 设置"，调用方会把它变成默认路由。
fn parse_proxy(value: &str) -> Option<Route> {
    let lower = value.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    // 限制：ureq 会说 HTTP 代理（CONNECT），但不会对代理本身说 *TLS*，所以一个
    // `https://` 代理会落进同一个"无法解析"的桶里并退化成 auto。
    if lower.starts_with("https://") {
        return None;
    }
    // 限制：代理凭据不会被百分号解码；ureq 原样接收它们。
    let proxy = ureq::Proxy::new(value.trim_end_matches('/')).ok()?;
    Some(Route {
        label: proxy_label(value),
        proxy: Some(proxy),
    })
}

/// 从代理 URL 取出的 `${hostname}:${port}`，端口的默认方式与
/// `parseProxy` 相同。
fn proxy_label(value: &str) -> String {
    let rest = value
        .split_once("://")
        .map(|(_, tail)| tail)
        .unwrap_or(value);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, tail)| tail)
        .unwrap_or(authority);
    if host_port.contains(':') {
        host_port.to_string()
    } else if value.to_ascii_lowercase().starts_with("https://") {
        format!("{}:443", host_port)
    } else {
        format!("{}:80", host_port)
    }
}

/// 针对某个设置值的有序候选：`auto` 是 [direct, clash]，被钉住的
/// 路由恰好是一个条目，而不可用的值会退化成 `auto`
/// （行为固定）。
fn resolve_routes(setting: &str) -> Vec<Route> {
    let value = setting.trim();
    if value.is_empty() || value == "auto" {
        return vec![direct_route(), clash_route()];
    }
    if value == "direct" {
        return vec![direct_route()];
    }
    if let Some(route) = parse_proxy(value) {
        return vec![route];
    }
    vec![direct_route(), clash_route()]
}

/// 通过一条路由做的、带 `Location` 跟随的一次 GET。
///
/// 非 2xx 的状态是一个*响应*：`Err` 只留给传输层问题（以及无法解析的
/// `Location`），而那正是 `requestWithEgress` 视为路由失败的东西。返回的错误
/// 文本已经过 [`describe_ureq_error`] 处理。
fn request_with_route(
    url: &str,
    route: &Route,
    timeout_ms: u64,
    user_agent: &str,
    headers: &[(&str, String)],
) -> Result<ureq::Response, String> {
    // 参考实现的 `req.setTimeout(ms)` 是*套接字空闲*超时，而不是整个传输的
    // 截止时间：一个 600 MB 的安装包绝不能在中途 60 s 被中止。
    // ureq 把各个阶段分开暴露，所以这里没有总的 `.timeout()`。
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .timeout_write(timeout)
        .redirects(0)
        .user_agent(user_agent);
    if let Some(proxy) = &route.proxy {
        builder = builder.proxy(proxy.clone());
    }
    let agent = builder.build();

    let mut current = url.to_string();
    let mut hops = 0u32;
    loop {
        let mut request = agent.get(&current);
        for (name, value) in headers {
            request = request.set(name, value);
        }
        // ureq 把 >=400 报告为 `Err(Status(_, response))`；把响应取回来，这样
        // 状态就能和其它任何应答一样被处理。
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(other) => return Err(describe_ureq_error(&other)),
        };

        let status = response.status();
        if !is_redirect(status) || hops >= MAX_REDIRECTS {
            return Ok(response);
        }
        let Some(location) = response.header("location") else {
            return Ok(response);
        };
        // 解析口径（涉及安全）：`requestWithRoute` 会跟随这一跳，而
        // 调用方只会通过 `assertDownloadable` 重新校验*原始* URL。
        // 因此重定向目标并不做主机检查。此处原样复现；见测试。
        current = resolve_redirect(&current, location)
            .ok_or_else(|| format!("cannot resolve the redirect target: {}", location))?;
        hops += 1;
    }
}

/// 直连路由上的一次 GET。供更新 API 使用，它的契约里没有代理
/// 设置。
pub(crate) fn request_with_redirects(
    url: &str,
    timeout_ms: u64,
    user_agent: &str,
    headers: &[(&str, String)],
) -> Result<ureq::Response, String> {
    request_with_route(url, &direct_route(), timeout_ms, user_agent, headers)
}

/* ------------------------------------------------------------------ */
/* 流式下载                                                             */
/* ------------------------------------------------------------------ */

/// 用更新器的传输策略，直接流式 GET 到 `dest`。
///
/// `proxy` 是原始的 `updateProxy` 设置（`"auto"` / `"direct"` / 一个 URL），
/// 与 `requestWithEgress` 一致：响应到达之前的传输失败会转到下一个候选，任何
/// 产生了响应的则不会。
///
/// * `on_progress(0, Some(content_length))` 在头部检查之后、文件被创建之前触发
///   一次（`updateProgress(0, declared)`），之后每次成功读取后都会带上当前字节数
///   触发。`received` 统计的是交给文件的字节数，不是块数；当 `Content-Length`
///   缺失时第二个元素是 `None`。
/// * 从 `on_progress` 返回 `false` 会中止（取消和体积上限都以这种方式从 manager
///   抵达）。部分文件会被留在磁盘上：由 `runDownload` 的 `catch` 决定删什么，
///   而它的取消路径是刻意保留它的。
/// * `max_bytes` 也在这里被强制执行：
///   声明的 `Content-Length` 会在写入任何字节之前被拒绝
///   （`` 安装包体积异常（N MB），已中止 ``），而流会在越过上限的第一个字节处
///   被切断（`` 安装包超过体积上限，已中止 ``）。
/// * 重定向最多跟随五跳，与参考实现相同。
/// * 状态必须恰好是 200，不是"任意 2xx"。
///
/// 传输失败被报告为 `` 无法连接服务器：… ``，与参考实现相同；响应到达之后的
/// 失败是 `` 下载中断: … ``。
pub fn download_to_file(
    url: &str,
    dest: &Path,
    proxy: &str,
    max_bytes: u64,
    timeout_ms: u64,
    on_progress: &mut dyn FnMut(u64, Option<u64>) -> bool,
) -> Result<u64, FetchError> {
    let headers = [
        ("accept", "application/octet-stream".to_string()),
        // 参考实现不发送 Accept-Encoding，所以安装包以存储时的样子抵达，
        // `received` 统计的正是 `Content-Length` 所声明的内容。
        // 否则 ureq 会宣告 gzip 并透明解压，那会同时破坏两个体积检查。
        ("accept-encoding", "identity".to_string()),
    ];
    let routes = resolve_routes(proxy);
    let mut last_error = None;
    let mut response = None;
    for route in &routes {
        match request_with_route(url, route, timeout_ms, UPDATER_USER_AGENT, &headers) {
            Ok(found) => {
                response = Some(found);
                break;
            }
            Err(message) => last_error = Some(message),
        }
    }
    let Some(response) = response else {
        return Err(FetchError::Transport(format!(
            "cannot reach the server: {}",
            last_error.unwrap_or_else(|| "unknown error".to_string())
        )));
    };

    let status = response.status();
    if status != 200 {
        // `response.status !== 200` —— 不是"非 2xx"。
        return Err(FetchError::Http(status));
    }

    let declared = parse_content_length(response.header("content-length"));
    if let Some(length) = declared {
        if length > max_bytes {
            return Err(FetchError::Transport(format!(
                "installer size looks wrong ({} MB); download aborted",
                (length as f64 / 1_048_576.0).round()
            )));
        }
    }

    if !on_progress(0, declared) {
        return Err(FetchError::Transport("download was aborted".to_string()));
    }

    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                FetchError::Transport(format!("failed to write the installer: {}", e))
            })?;
        }
    }
    let mut file = std::fs::File::create(dest)
        .map_err(|e| FetchError::Transport(format!("failed to write the installer: {}", e)))?;

    let mut reader = response.into_reader();
    let mut buffer = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(FetchError::Transport(format!(
                    "download interrupted: {}",
                    describe_io_error(&e)
                )))
            }
        };
        received += read as u64;
        if received > max_bytes {
            return Err(FetchError::Transport(
                "installer exceeds the size cap; download aborted".to_string(),
            ));
        }
        // 进度在字节抵达文件之前就被报告，与 `file.write(chunk)` 之前的那次
        // `updateProgress(...)` 调用相对应。
        if !on_progress(received, declared) {
            return Err(FetchError::Transport("download was aborted".to_string()));
        }
        file.write_all(&buffer[..read])
            .map_err(|e| FetchError::Transport(format!("failed to write the installer: {}", e)))?;
    }

    file.flush()
        .map_err(|e| FetchError::Transport(format!("failed to write the installer: {}", e)))?;
    Ok(received)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_classification_is_stable() {
        assert!(FetchError::Timeout.is_timeout());
        assert!(!FetchError::Http(404).is_timeout());
        assert_eq!(FetchError::Http(404).describe(), "http 404");
    }

    #[test]
    fn refused_connection_is_transport_not_http() {
        // 回环地址上的 1 端口：连接被拒，不涉及 DNS。
        match get_json("http://127.0.0.1:1/", 1000) {
            Err(FetchError::Transport(_)) => {}
            other => panic!("expected transport error, got {:?}", other),
        }
    }

    /* ---- download_to_file ------------------------------------------- */

    use std::net::TcpListener;

    /// 在一个回环端口上按顺序供应 `responses`，每个接受的连接一个。永不 join：
    /// 线程在响应发完或 10 秒后停止。
    ///
    /// 有两处在 Windows 上是承重的；任何一处弄错都会让这些测试间歇性失败
    /// （在满负荷并行 `cargo test` 下大约 5 次里有 1 次，单独跑则从不）：
    ///
    /// * **先把整个请求读完。** ureq 按套接字给它的分块方式发送请求行和头部，
    ///   而客户端在服务器回复之后仍会继续写入。关闭一个仍有未读入站字节的套接字
    ///   会让 Windows 发出 RST 而不是 FIN，从而丢掉客户端尚未读到的响应 ——
    ///   表现为 `os error 10053` / "Network Error"，而不是一个错误的响应。
    /// * **先半关闭，再排空。** `shutdown(Write)` 发出 FIN，好让读到 EOF 的客户端
    ///   （那个没有 `Content-Length` 的测试）看到一个干净的结束，随后的排空则吸收
    ///   客户端仍在发送的内容，然后套接字才被丢弃。
    fn serve(responses: Vec<String>) -> String {
        use std::io::Write as _;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut index = 0;
            while index < responses.len() && std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // 在 Windows 上，被接受的套接字会*继承*监听套接字的非阻塞
                        // 模式，而非阻塞套接字会忽略 SO_RCVTIMEO：`read` 立刻返回
                        // WouldBlock。在满负荷的 `cargo test` 下，客户端会在
                        // connect() 和 write() 之间被调度出去，于是这个桩看到了
                        // WouldBlock、带着尚未写完的请求跳出读取循环并关闭。
                        // 客户端的字节随后落在正在被拆除的套接字上，Windows 用 RST
                        // 而不是 FIN 作答，客户端的状态行读取以
                        // `os error 10053` 告终 —— 这就是只在完整套件里出现、
                        // 单独跑从不出现的间歇性失败。
                        // 把套接字恢复成阻塞模式，好让下面的超时真的能等待。
                        stream.set_nonblocking(false).ok();
                        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                        // 一直读到请求头完整。单次 `read` 在客户端把写入分块时
                        // 可能只返回请求行；此时回复会让头的其余部分仍留在入站，
                        // 而关闭一个带未读入站字节的 Windows 套接字会发出 RST，
                        // 从而丢掉客户端尚未读到的回复。
                        // 那就是在并行 `cargo test` 下间歇出现的
                        // `os error 10053` / "Error encountered in the status line"。
                        //
                        // 超时或对端关闭会打破循环，而回复仍会写出：这是一个桩
                        // 服务器，一个被截断的 GET 头不改变它所应答的内容。
                        let mut request = Vec::new();
                        let mut buf = [0u8; 1024];
                        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                            match stream.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => request.extend_from_slice(&buf[..n]),
                                Err(ref e)
                                    if e.kind() == std::io::ErrorKind::WouldBlock
                                        || e.kind() == std::io::ErrorKind::TimedOut =>
                                {
                                    break
                                }
                                Err(_) => break,
                            }
                        }
                        let _ = stream.write_all(responses[index].as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                        // 在套接字被丢弃之前吸收客户端仍在发送的内容，好让关闭
                        // 是 FIN 而不是 RST。限定为一次短暂的读取：客户端已经拿到
                        // 了它的回复，所以剩下的内容就是它自己的收尾动作。
                        stream
                            .set_read_timeout(Some(Duration::from_millis(50)))
                            .ok();
                        let mut sink = [0u8; 1024];
                        let _ = stream.read(&mut sink);
                        index += 1;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        format!("http://{}", addr)
    }

    fn http_response(status: u16, headers: &[(&str, &str)], body: &str) -> String {
        let mut out = format!("HTTP/1.1 {} X\r\n", status);
        for (name, value) in headers {
            out.push_str(&format!("{}: {}\r\n", name, value));
        }
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
        out.push_str("Connection: close\r\n\r\n");
        out.push_str(body);
        out
    }

    /// 没有 `Content-Length`：正文在 EOF 处结束（`Connection: close`）。
    fn http_response_streamed(status: u16, body: &str) -> String {
        format!("HTTP/1.1 {} X\r\nConnection: close\r\n\r\n{}", status, body)
    }

    /// 一个一次性的目标位置，它的父目录只属于本次调用。
    ///
    /// 光有时间戳还不够：六个下载测试会在同一时刻启动，而一个进程里的两次
    /// `SystemTime::now()` 调用在 Windows 上可能解析到同一个 100 ns 刻度，于是
    /// 两个测试拿到同一个父目录 —— 接着一个测试的 `remove_dir_all(parent)` 清理
    /// 会在 `create_dir_all` 与 `File::create` 之间删掉另一个测试的目标位置，
    /// 表现为 `os error 3`（"系统找不到指定的路径"）。在 20 次并发的完整套件
    /// 运行中见过一次，单独跑时从不出现。这个计数器消除碰撞，而不是去放宽
    /// 时间戳。
    fn temp_dest(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir()
            .join(format!(
                "ak-http-test-{}-{}-{}",
                std::process::id(),
                stamp,
                seq
            ))
            .join(name)
    }

    #[test]
    fn download_writes_bytes_and_reports_progress() {
        let url = serve(vec![http_response(
            200,
            &[("Content-Type", "application/octet-stream")],
            "hello installer",
        )]);
        let dest = temp_dest("a.exe");
        let mut events: Vec<(u64, Option<u64>)> = Vec::new();

        let written =
            download_to_file(&url, &dest, "direct", 1024, 5000, &mut |received, total| {
                events.push((received, total));
                true
            })
            .unwrap();

        assert_eq!(written, 15);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello installer");
        // 第一个事件是写入前的 `updateProgress(0, declared)`；最后一个
        // 带着完整的大小。
        assert_eq!(events.first(), Some(&(0, Some(15))));
        assert_eq!(events.last(), Some(&(15, Some(15))));
        let mut previous = 0;
        for (received, total) in &events {
            assert_eq!(*total, Some(15));
            assert!(*received >= previous);
            previous = *received;
        }
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn download_rejects_declared_oversize_before_writing() {
        let url = serve(vec![http_response(200, &[], "0123456789")]);
        let dest = temp_dest("declared.exe");

        let error = download_to_file(&url, &dest, "direct", 5, 5000, &mut |_, _| true).unwrap_err();
        match error {
            // `Math.round(bytes / 1048576)`。
            FetchError::Transport(message) => {
                assert!(message.contains("size looks wrong"), "{}", message);
                assert!(message.contains("0 MB"), "{}", message);
            }
            other => panic!("expected transport error, got {:?}", other),
        }
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn download_cuts_the_stream_off_without_content_length() {
        let url = serve(vec![http_response_streamed(200, "0123456789")]);
        let dest = temp_dest("streamed.exe");

        let error = download_to_file(&url, &dest, "direct", 5, 5000, &mut |_, _| true).unwrap_err();
        match error {
            FetchError::Transport(message) => {
                assert!(message.contains("exceeds the size cap"), "{}", message)
            }
            other => panic!("expected transport error, got {:?}", other),
        }
        // 传输层不会删除：由 `runDownload` 的 catch 决定，而它的取消路径是刻意
        // 保留这个部分文件的。
        assert!(dest.exists());
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn progress_returning_false_aborts_before_the_file_is_created() {
        let url = serve(vec![http_response(200, &[], "payload")]);
        let dest = temp_dest("cancelled.exe");

        let error =
            download_to_file(&url, &dest, "direct", 1024, 5000, &mut |_, _| false).unwrap_err();
        assert!(matches!(error, FetchError::Transport(_)));
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn routes_follow_the_declared_policy() {
        assert_eq!(resolve_routes("auto").len(), 2);
        assert_eq!(resolve_routes("").len(), 2);
        assert_eq!(resolve_routes("direct").len(), 1);
        assert_eq!(resolve_routes("direct")[0].label, "direct connection");
        assert!(resolve_routes("direct")[0].proxy.is_none());

        let pinned = resolve_routes("http://127.0.0.1:8888");
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].label, "127.0.0.1:8888");
        assert!(pinned[0].proxy.is_some());

        // 不可用的值会退化成 auto，而不是把更新搞瘫。
        assert_eq!(resolve_routes("not a url").len(), 2);
        // 记录在案的限制：ureq 无法对代理做 TLS。
        assert_eq!(resolve_routes("https://proxy.local:8443").len(), 2);
    }

    #[test]
    fn download_follows_a_relative_redirect() {
        let url = serve(vec![
            http_response(302, &[("Location", "/installer")], ""),
            http_response(200, &[], "payload"),
        ]);
        let dest = temp_dest("redirected.exe");

        let written =
            download_to_file(&url, &dest, "direct", 1024, 5000, &mut |_, _| true).unwrap();
        assert_eq!(written, 7);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "payload");
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn download_surfaces_non_200_as_http() {
        let url = serve(vec![http_response(404, &[], "no")]);
        let dest = temp_dest("missing.exe");

        match download_to_file(&url, &dest, "direct", 1024, 5000, &mut |_, _| true) {
            Err(FetchError::Http(404)) => {}
            other => panic!("expected http 404, got {:?}", other),
        }
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn redirect_locations_resolve_like_new_url() {
        let base = "https://github.com/owner/repo/releases/download/v1/setup.exe";
        assert_eq!(
            resolve_redirect(base, "https://objects.githubusercontent.com/x").as_deref(),
            Some("https://objects.githubusercontent.com/x")
        );
        assert_eq!(
            resolve_redirect(base, "//cdn.example.com/x").as_deref(),
            Some("https://cdn.example.com/x")
        );
        assert_eq!(
            resolve_redirect(base, "/absolute").as_deref(),
            Some("https://github.com/absolute")
        );
        assert_eq!(
            resolve_redirect(base, "sibling.exe").as_deref(),
            Some("https://github.com/owner/repo/releases/download/v1/sibling.exe")
        );
        assert_eq!(
            resolve_redirect("https://github.com/a?x=1", "?y=2").as_deref(),
            Some("https://github.com/a?y=2")
        );
        assert_eq!(resolve_redirect(base, ""), None);
    }
}
