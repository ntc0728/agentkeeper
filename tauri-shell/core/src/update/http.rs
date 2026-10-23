//! Update-specific transport policy.
//!
//! The egress policy (`auto` → direct then the local Clash mixed port, a
//! pinned proxy URL) lives in [`crate::http`]; this module takes no
//! proxy setting of its own, so only the direct route exists here.
//!
//! What this module implements:
//!
//! * the GitHub header set;
//! * redirects with the same five-hop budget;
//! * the 4 MiB JSON body ceiling (`MAX_JSON_BYTES`, `readBody`);
//! * `describeError`'s transport messages, because the UI shows them verbatim.

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;

use super::version::UpdateError;
use crate::http::request_with_redirects;

/// JSON responses are tiny; anything larger is treated as a broken response
/// （4 MiB 上限）。
pub const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;

/// `USER_AGENT` — 固定的字符串。
const USER_AGENT: &str = "AgentKeeper-updater";

/// One response from the API, before the manager's interpretation.
///
/// `getJson` 返回 `{ status, headers, json }`，manager 三者都要看
/// branches on all three (404 = no release, 403/429 = rate limit with a
/// deadline read from the headers). A body-only return value cannot express
/// those states.
#[derive(Debug, Clone)]
pub struct ReleaseFetch {
    pub status: u16,
    /// Lowercased header names.
    pub headers: BTreeMap<String, String>,
    /// `None` for a non-2xx response, mirroring `json = null`.
    pub json: Option<Value>,
}

/// `githubHeaders` 的头部集合。`authorization` is present
/// only when a token exists — an empty string is falsy, so it is
/// skipped here too.
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
    // `readBody`：accumulate, fail as soon as the
    // running total passes the limit.
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

/// `getJson`，in the shape the manager needs:
/// `(status, headers, json)`.
///
/// * `Ok` for **any** HTTP status, because 404 ("no release yet") and 403/429
///   ("rate limited") are answers, not transport failures — the manager reads
///   the status and the rate-limit headers to decide.
/// * `Err` only for transport failures and an unparseable 2xx body.
/// * `json` is `Value::Null` for a non-2xx response (`json = null`).
/// * Headers are lowercased, one value per name, like `res.headers`
///   (the manager reads `retry-after` and `x-ratelimit-*`).
pub fn fetch_release(
    url: &str,
    token: Option<&str>,
    timeout_ms: u64,
) -> Result<ReleaseFetch, UpdateError> {
    let headers = github_headers(token);
    let response =
        request_with_redirects(url, timeout_ms, USER_AGENT, &headers).map_err(|text| {
            // `throw new UpdateError('无法连接服务器：' + describeError(...))`
            // default status 502.
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
        // `response.res.resume()` — the body is dropped.
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

/// The narrow convenience wrapper: a 2xx body, or an `UpdateError`.
///
/// Because the three branches are collapsed into
/// `Result<Value>`, the mapping is:
///
/// * 2xx → the parsed body;
/// * 404 → `Ok(Value::Null)` — "no release", not a
///   failed check;
/// * 403 / 429 → the rate-limit message;
/// * any other non-2xx → `` GitHub 返回 HTTP {status} ``;
/// * transport failures → `` 无法连接服务器：… ``.
///
/// The `UpdateError` status is 502 in every case, which is the reference
/// default; the HTTP status itself stays available through [`fetch_release`].
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

    /// Serve `responses` in order, one per accepted connection. Detached.
    ///
    /// Reads once, replies, half-closes, then drains — see the long note on
    /// `core::http::tests::serve`. Closing a Windows socket that still has
    /// unread inbound bytes sends RST and discards the unread reply, which is
    /// what made these tests fail ~1 run in 20 under a parallel `cargo test`
    /// and never in isolation.
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
                        // Accepted sockets inherit the listener's nonblocking
                        // mode on Windows, where SO_RCVTIMEO is then ignored and
                        // `read` returns WouldBlock immediately — the stub used
                        // to answer (and close) before the descheduled client had
                        // even written its request, which Windows turns into an
                        // RST and the client sees as `os error 10053`. Blocking +
                        // a real timeout is what makes this stub wait for the
                        // request. See the longer note in `core::http::tests::serve`.
                        stream.set_nonblocking(false).ok();
                        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                        // Read until the request head is complete — see the
                        // note in `core::http::tests::serve`. A single read can
                        // return just the request line; replying then leaves
                        // inbound bytes unread, and closing such a socket on
                        // Windows sends RST and discards the reply.
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
                        // Absorb whatever is still inbound so the close is a FIN.
                        // One short pass only: the client already has its reply.
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
        // An empty token is falsy and must not produce a header.
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
        // Two requests below, so the mock listener needs two responses: `serve`
        // stops accepting once its queue is drained, and a later connect is
        // refused (a real transport failure, not a non-2xx answer).
        let answer = http_response(404, &[("Retry-After", "0")], "{}");
        let url = serve(vec![answer.clone(), answer]);
        let response = fetch_release(&url, None, 5000).unwrap();
        assert_eq!(response.status, 404);
        assert!(response.json.is_none());
        assert!(response.headers.contains_key("retry-after"));

        // The narrow wrapper maps 404 to "no release".
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

        // Six redirects: five are followed, the sixth response is returned, so
        // the caller sees a 3xx（口径同 `depth < 5`）。
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
        // Port 1: refused before any response, so this is a transport failure
        // and never an Ok.
        let error = fetch_release("http://127.0.0.1:1/x", None, 1000).unwrap_err();
        assert_eq!(error.status(), 502);
        assert!(
            error.message().starts_with("cannot reach the server:"),
            "{}",
            error
        );
    }
}
