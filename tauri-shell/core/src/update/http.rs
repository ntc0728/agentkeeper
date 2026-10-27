//! 更新专用的传输策略。
//!
//! 出口策略（`auto` → 先直连、再走本机 Clash mixed port，或固定的代理 URL）
//! 位于 [`crate::http`]；本模块不持有自己的代理设置，因此这里只有直连路由。
//!
//! 本模块实现：
//!
//! * GitHub 请求头集合；
//! * 重定向，同样是五跳预算；
//! * 4 MiB JSON body 上限（`MAX_JSON_BYTES`、`readBody`）；
//! * `describeError` 的传输层消息，因为 UI 会原样显示它们。

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;

use super::version::UpdateError;
use crate::http::request_with_redirects;

/// JSON 响应很小；更大的都被当作损坏的响应
/// （4 MiB 上限）。
pub const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;

/// `USER_AGENT` — 固定的字符串。
const USER_AGENT: &str = "AgentKeeper-updater";

/// 来自 API 的一次响应，在 manager 解读之前。
///
/// `getJson` 返回 `{ status, headers, json }`，manager 三者都要看
/// —— 三条分支都要用到（404 = 没有发布，403/429 = 触发限流，
/// 截止时间从响应头读出）。只返回 body 无法表达这些状态。
#[derive(Debug, Clone)]
pub struct ReleaseFetch {
    pub status: u16,
    /// 小写的响应头名。
    pub headers: BTreeMap<String, String>,
    /// 非 2xx 响应时为 `None`，与 `json = null` 一致。
    pub json: Option<Value>,
}

/// `githubHeaders` 的头部集合。`authorization` 仅在
/// 存在令牌时才出现 —— 空字符串是假值，因此这里也会跳过它。
fn github_headers(token: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("accept", "application/vnd.github+json".to_string()),
        ("user-agent", USER_AGENT.to_string()),
        ("x-github-api-version", "2022-11-28".to_string()),
    ];
    if let Some(token) = token {
        if !token.is_empty() {
            headers.push(("authorization", format!("Bearer {}", token)));
        }
    }
    headers
}

fn read_body_capped(reader: &mut dyn Read) -> Result<Vec<u8>, UpdateError> {
    // `readBody`：累积读取，一旦累计总量超过上限就失败。
    let mut body = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut size: u64 = 0;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                size += read as u64;
                if size > MAX_JSON_BYTES {
                    return Err(UpdateError::new(
                        format!("response body exceeds the limit ({} bytes)", MAX_JSON_BYTES),
                        502,
                    ));
                }
                body.extend_from_slice(&buffer[..read]);
            }
            Err(error) => {
                return Err(UpdateError::new(
                    format!("cannot reach the server: {}", error),
                    502,
                ))
            }
        }
    }
    Ok(body)
}

/// 即参考实现的 `getJson`，但采用 manager 需要的形状：
/// 返回三元组 `(status, headers, json)`。
///
/// * 任何 HTTP 状态都返回 `Ok`，因为 404（"还没有发布"）和 403/429
///   （"被限流"）是答案而非传输失败 —— manager 读取状态码与限流响应头来决策。
/// * 只有传输失败和无法解析的 2xx body 才返回 `Err`。
/// * 非 2xx 响应时 `json` 为 `Value::Null`（`json = null`）。
/// * 响应头一律小写，每个名字一个值，与 `res.headers` 一致
///   （manager 会读 `retry-after` 和 `x-ratelimit-*`）。
pub fn fetch_release(
    url: &str,
    token: Option<&str>,
    timeout_ms: u64,
) -> Result<ReleaseFetch, UpdateError> {
    let headers = github_headers(token);
    let response =
        request_with_redirects(url, timeout_ms, USER_AGENT, &headers).map_err(|text| {
            // `throw new UpdateError('无法连接服务器：' + describeError(...))`
            // 默认状态码 502。
            UpdateError::new(format!("cannot reach the server: {}", text), 502)
        })?;

    let status = response.status();
    let mut response_headers = BTreeMap::new();
    for name in response.headers_names() {
        if let Some(value) = response.header(&name) {
            response_headers.insert(name.to_lowercase(), value.to_string());
        }
    }

    if !(200..300).contains(&status) {
        // `response.res.resume()` —— body 被丢弃。
        return Ok(ReleaseFetch {
            status,
            headers: response_headers,
            json: None,
        });
    }

    let mut reader = response.into_reader();
    let body = read_body_capped(&mut reader)?;
    let json = serde_json::from_slice::<Value>(&body).map_err(|error| {
        // `解析响应失败: ${e.message}`。
        UpdateError::new(format!("failed to parse the response: {}", error), 502)
    })?;

    Ok(ReleaseFetch {
        status,
        headers: response_headers,
        json: Some(json),
    })
}

/// 窄包装：2xx 的 body，或一个 `UpdateError`。
///
/// 因为三条分支被折叠进
/// `Result<Value>`，映射关系是：
///
/// * 2xx → 解析后的 body；
/// * 404 → `Ok(Value::Null)` —— "没有发布"，不是
///   检查失败；
/// * 403 / 429 → 限流消息；
/// * 其他任何非 2xx → `` GitHub 返回 HTTP {status} ``；
/// * 传输失败 → `` 无法连接服务器：… ``。
///
/// 各种情况下 `UpdateError` 的状态码都是 502，这是参考实现的
/// 默认值；HTTP 状态码本身仍可通过 [`fetch_release`] 取得。
pub fn fetch_release_json(
    url: &str,
    token: Option<&str>,
    timeout_ms: u64,
) -> Result<Value, UpdateError> {
    let response = fetch_release(url, token, timeout_ms)?;
    if (200..300).contains(&response.status) {
        return Ok(response.json.unwrap_or(Value::Null));
    }
    match response.status {
        404 => Ok(Value::Null),
        403 | 429 => Err(UpdateError::new(
            "GitHub API rate limit reached; checks are paused and the last result kept. Add a GitHub token in update settings to raise the limit",
            502,
        )),
        status => Err(UpdateError::new(format!("GitHub returned HTTP {}", status), 502)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Duration;

    /// 按顺序供应 `responses`，每次接受的连接一个。独立线程运行。
    ///
    /// 读一次、回复、半关闭，然后排空 —— 见 `core::http::tests::serve` 中的
    /// 长注记。关闭一个仍有未读入站字节的 Windows socket 会发 RST 并丢弃
    /// 未读的回复，这正是并行 `cargo test` 下这些测试约每 20 次失败 1 次、
    /// 而单独运行时从不超过的原因。
    fn serve(responses: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut index = 0;
            while index < responses.len() && std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // 在 Windows 上被接受的 socket 会继承监听器的非阻塞
                        // 模式，此时 SO_RCVTIMEO 被忽略，`read` 立刻返回
                        // WouldBlock —— 过去使用的桩会在被调度出去的客户端
                        // 甚至还没写出请求时就回复（并关闭），Windows 会把这
                        // 变成 RST，客户端看到的就是 `os error 10053`。改为阻塞
                        // 加真实超时，才让这个桩等到请求。详见
                        // `core::http::tests::serve` 中更长的注记。
                        stream.set_nonblocking(false).ok();
                        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                        // 一直读到请求头完整 —— 见
                        // `core::http::tests::serve` 中的注记。单次 read 可能
                        // 只返回请求行；此时回复会让入站字节未被读取，而在
                        // Windows 上关闭这类 socket 会发 RST 并丢弃回复。
                        let mut request = Vec::new();
                        let mut buf = [0u8; 1024];
                        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                            match std::io::Read::read(&mut stream, &mut buf) {
                                Ok(0) => break,
                                Ok(n) => request.extend_from_slice(&buf[..n]),
                                Err(_) => break,
                            }
                        }
                        let _ = std::io::Write::write_all(&mut stream, responses[index].as_bytes());
                        let _ = std::io::Write::flush(&mut stream);
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                        // 吸收掉剩余的入站数据，使关闭变成 FIN。
                        // 只做一次短暂的读取：客户端已经拿到回复了。
                        stream
                            .set_read_timeout(Some(Duration::from_millis(50)))
                            .ok();
                        let mut sink = [0u8; 1024];
                        let _ = std::io::Read::read(&mut stream, &mut sink);
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

    #[test]
    fn github_headers_add_authorization_only_with_a_token() {
        let plain = github_headers(None);
        let names: Vec<&str> = plain.iter().map(|(name, _)| *name).collect();
        assert!(names.contains(&"accept"));
        assert!(names.contains(&"user-agent"));
        assert!(names.contains(&"x-github-api-version"));
        assert!(!names.contains(&"authorization"));
        assert_eq!(
            plain
                .iter()
                .find(|(name, _)| *name == "accept")
                .map(|(_, value)| value.as_str()),
            Some("application/vnd.github+json")
        );

        let with_token = github_headers(Some("ghp_example"));
        assert!(with_token.iter().any(
            |(name, value)| *name == "authorization" && value.as_str() == "Bearer ghp_example"
        ));
        // 空令牌是假值，不应产生该请求头。
        assert!(!github_headers(Some(""))
            .iter()
            .any(|(name, _)| *name == "authorization"));
    }

    #[test]
    fn parses_a_json_body_and_keeps_the_status() {
        let url = serve(vec![http_response(
            200,
            &[("X-RateLimit-Remaining", "4999")],
            r#"{"tag_name":"v1.2.3"}"#,
        )]);
        let response = fetch_release(&url, None, 5000).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.json.unwrap()["tag_name"], "v1.2.3");
        assert_eq!(
            response
                .headers
                .get("x-ratelimit-remaining")
                .map(String::as_str),
            Some("4999")
        );
    }

    #[test]
    fn non_2xx_is_a_response_not_an_error() {
        // 下面两次请求，所以 mock 监听器需要两个响应：`serve`
        // 在队列排空后不再接受连接，之后的连接会被拒绝（那是真实的传输失败，
        // 而不是非 2xx 的答案）。
        let answer = http_response(404, &[("Retry-After", "0")], "{}");
        let url = serve(vec![answer.clone(), answer]);
        let response = fetch_release(&url, None, 5000).unwrap();
        assert_eq!(response.status, 404);
        assert!(response.json.is_none());
        assert!(response.headers.contains_key("retry-after"));

        // 窄包装把 404 映射为"没有发布"。
        assert_eq!(fetch_release_json(&url, None, 5000).unwrap(), Value::Null);
    }

    #[test]
    fn wrapper_turns_other_statuses_into_update_errors() {
        let url = serve(vec![http_response(500, &[], "boom")]);
        let error = fetch_release_json(&url, None, 5000).unwrap_err();
        assert_eq!(error.status(), 502);
        assert_eq!(error.message(), "GitHub returned HTTP 500");

        let url = serve(vec![http_response(403, &[], "{}")]);
        let error = fetch_release_json(&url, None, 5000).unwrap_err();
        assert!(error.message().contains("rate limit"), "{}", error);
    }

    #[test]
    fn follows_redirects_up_to_the_hop_limit() {
        let url = serve(vec![
            http_response(301, &[("Location", "/next")], ""),
            http_response(200, &[], r#"{"ok":true}"#),
        ]);
        let response = fetch_release(&url, None, 5000).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.json.unwrap()["ok"], true);

        // 六次重定向：跟随其中五次，第六个响应被返回，因此
        // 调用方看到的是 3xx（口径同 `depth < 5`）。
        let mut responses = Vec::new();
        for location in ["/a", "/b", "/c", "/d", "/e", "/f"] {
            responses.push(http_response(302, &[("Location", location)], ""));
        }
        let url = serve(responses);
        let response = fetch_release(&url, None, 5000).unwrap();
        assert_eq!(response.status, 302);
        assert!(response.json.is_none());
    }

    #[test]
    fn enforces_the_json_body_ceiling() {
        let data = vec![b'x'; (MAX_JSON_BYTES + 1) as usize];
        let mut reader: &[u8] = &data;
        let error = read_body_capped(&mut reader).unwrap_err();
        assert!(error.message().contains("exceeds the limit"), "{}", error);

        let mut small: &[u8] = b"{}";
        assert_eq!(read_body_capped(&mut small).unwrap(), b"{}");
    }

    #[test]
    fn invalid_json_is_a_parse_error() {
        let url = serve(vec![http_response(200, &[], "not json")]);
        let error = fetch_release(&url, None, 5000).unwrap_err();
        assert!(error.message().starts_with("failed to parse"), "{}", error);
    }

    #[test]
    fn transport_failures_name_the_route_problem() {
        // 1 号端口：在任何响应之前就被拒绝，因此这是传输失败，
        // 绝不会返回 Ok。
        let error = fetch_release("http://127.0.0.1:1/x", None, 1000).unwrap_err();
        assert_eq!(error.status(), 502);
        assert!(
            error.message().starts_with("cannot reach the server:"),
            "{}",
            error
        );
    }
}
