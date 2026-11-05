// 接口测试工具（api_request）：单发 HTTP 请求 + 内置并发压测。
//
// 测试工作区不装 shell（测需求不碰代码），接口与并发测试由本工具承担：
// - 单发（concurrency 缺省 1）：返回状态码/响应头摘要/响应体——接口用例的断言依据；
// - 并发（concurrency>1）：tokio 同时发 total 个请求，返回成功/失败统计与
//   耗时分布——不写脚本就能做并发测试（模型串行连发多次单请求测不了真并发）。
//
// 能力面刻意收窄：只有 HTTP 请求，没有文件/shell——与「测试不负责代码」的
// 职责边界一致；url 由模型按被测系统地址给出，工具不做白名单（测试环境
// 本就是用户自己的本机/内网）。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use agentscope_core::error::Result;
use agentscope_core::tool::{Tool, ToolOutput};

/// 单发响应体截断（并发模式不返回全量 body，只取失败样本片段）。
const MAX_BODY: usize = 8 * 1024;
const MAX_CONCURRENCY: usize = 200;
const MAX_TOTAL: usize = 1000;

pub struct ApiRequest {
    client: reqwest::Client,
}

impl Default for ApiRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiRequest {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

fn input_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

fn input_u64(input: &Value, key: &str) -> Option<u64> {
    input.get(key).and_then(|v| v.as_u64())
}

/// 发一个请求，返回 (状态码, 响应头, 响应体)。网络层错误转 String 样本。
async fn send_one(
    client: &reqwest::Client,
    method: &str,
    url: &str,
    headers: Option<&Value>,
    body: Option<&str>,
    timeout: Duration,
) -> std::result::Result<(u16, String, String), String> {
    let m = method
        .parse::<reqwest::Method>()
        .map_err(|e| format!("非法 method `{method}`: {e}"))?;
    let mut req = client.request(m, url).timeout(timeout);
    if let Some(Value::Object(map)) = headers {
        for (k, v) in map {
            let v = v.as_str().ok_or_else(|| format!("header `{k}` 的值必须是字符串"))?;
            req = req.header(k.as_str(), v);
        }
    }
    if let Some(b) = body {
        if !b.is_empty() {
            req = req.body(b.to_string());
        }
    }
    let resp = req.send().await.map_err(|e| format!("{e}"))?;
    let status = resp.status().as_u16();
    // 响应头只留常用的几个（全量太长，断言用不到）。
    let mut hdr_lines = Vec::new();
    for (k, v) in resp.headers() {
        let name = k.as_str();
        if matches!(name, "content-type" | "content-length" | "location" | "set-cookie") {
            if let Ok(v) = v.to_str() {
                hdr_lines.push(format!("{name}: {v}"));
            }
        }
    }
    let text = resp.text().await.map_err(|e| format!("读响应体失败: {e}"))?;
    Ok((status, hdr_lines.join("\n"), text))
}

fn truncate_body(text: &str) -> String {
    if text.len() <= MAX_BODY {
        text.to_string()
    } else {
        let mut end = MAX_BODY;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n…（响应体已截断，共 {} 字节）", &text[..end], text.len())
    }
}

#[async_trait::async_trait]
impl Tool for ApiRequest {
    fn name(&self) -> &str {
        "api_request"
    }

    fn description(&self) -> &str {
        "Send an HTTP request to the system under test — API testing without shell. \
Single request (default) returns status/headers/body for assertions. \
Set concurrency>1 for load testing: the tool fires `total` requests concurrently \
and returns success/failure counts, status distribution and latency percentiles \
(no scripts needed)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"], "description": "HTTP method. Default GET."},
                "url": {"type": "string", "description": "Absolute URL of the system under test."},
                "headers": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Optional request headers (Authorization, Content-Type...)."},
                "body": {"type": "string", "description": "Optional request body (raw text; for JSON APIs pass the JSON text and set Content-Type header)."},
                "concurrency": {"type": "integer", "description": "Concurrent in-flight requests. Default 1 (single request). Max 200."},
                "total": {"type": "integer", "description": "Total requests to send in concurrency mode. Default = concurrency. Max 1000."},
                "timeout_secs": {"type": "integer", "description": "Per-request timeout in seconds. Default 30, max 120."}
            },
            "required": ["url"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let url = match input_str(&input, "url") {
            Some(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => return Ok(ToolOutput::error("url 不能为空")),
        };
        let method = input_str(&input, "method").unwrap_or("GET").trim().to_uppercase();
        let headers = input.get("headers").cloned();
        let body = input_str(&input, "body").map(|s| s.to_string());
        let concurrency = input_u64(&input, "concurrency").unwrap_or(1).clamp(1, MAX_CONCURRENCY as u64) as usize;
        let total = input_u64(&input, "total")
            .unwrap_or(concurrency as u64)
            .clamp(1, MAX_TOTAL as u64) as usize;
        let timeout = Duration::from_secs(input_u64(&input, "timeout_secs").unwrap_or(30).clamp(1, 120));

        // 单发：完整响应（状态码 + 头摘要 + body）。
        if concurrency == 1 && total <= 1 {
            return match send_one(&self.client, &method, &url, headers.as_ref(), body.as_deref(), timeout).await {
                Ok((status, hdrs, text)) => Ok(ToolOutput::ok(format!(
                    "HTTP {status}\n{hdrs}\n\n{}",
                    truncate_body(&text)
                ))),
                Err(e) => Ok(ToolOutput::error(format!("请求失败：{e}"))),
            };
        }

        // 并发压测：JoinSet 同时在途，收集统计。
        let start = Instant::now();
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..total {
            let client = self.client.clone();
            let method = method.clone();
            let url = url.clone();
            let headers = headers.clone();
            let body = body.clone();
            set.spawn(async move {
                let t0 = Instant::now();
                let r = send_one(&client, &method, &url, headers.as_ref(), body.as_deref(), timeout).await;
                (t0.elapsed(), r)
            });
        }
        let mut success = 0usize;
        let mut failed = 0usize;
        let mut latencies_ms: Vec<u128> = Vec::new();
        let mut status_dist: BTreeMap<u16, usize> = BTreeMap::new();
        let mut samples: Vec<String> = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((elapsed, Ok((status, _, text)))) => {
                    latencies_ms.push(elapsed.as_millis());
                    *status_dist.entry(status).or_default() += 1;
                    if (200..400).contains(&status) {
                        success += 1;
                    } else {
                        failed += 1;
                        if samples.len() < 3 {
                            samples.push(format!("HTTP {status}：{}", text.chars().take(200).collect::<String>()));
                        }
                    }
                }
                Ok((_, Err(e))) => {
                    failed += 1;
                    if samples.len() < 3 {
                        samples.push(e);
                    }
                }
                Err(e) => {
                    failed += 1;
                    if samples.len() < 3 {
                        samples.push(format!("任务异常：{e}"));
                    }
                }
            }
        }
        let wall = start.elapsed().as_millis();
        latencies_ms.sort_unstable();
        let pick = |p: usize| -> u128 {
            if latencies_ms.is_empty() {
                0
            } else {
                latencies_ms[(p * latencies_ms.len() / 100).min(latencies_ms.len() - 1)]
            }
        };
        let avg = if latencies_ms.is_empty() {
            0
        } else {
            latencies_ms.iter().sum::<u128>() / latencies_ms.len() as u128
        };
        let dist = status_dist
            .iter()
            .map(|(s, n)| format!("{s}×{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut out = format!(
            "并发压测完成：total={total} concurrency={concurrency} 用时 {wall}ms\n\
             成功（2xx/3xx）{success}  失败 {failed}\n\
             状态码分布：{dist}\n\
             耗时 ms：min={} avg={avg} p50={} p95={} max={}",
            latencies_ms.first().copied().unwrap_or(0),
            pick(50),
            pick(95),
            latencies_ms.last().copied().unwrap_or(0),
        );
        if !samples.is_empty() {
            out.push_str("\n失败样本（最多 3 条）：");
            for s in samples {
                out.push_str(&format!("\n- {s}"));
            }
        }
        Ok(ToolOutput::ok(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::ToolResultState;
    use std::io::{Read, Write};

    fn ok_text(out: ToolOutput) -> String {
        assert_eq!(out.state, ToolResultState::Success, "expected success: {}", out.content);
        out.content
    }

    /// 极简本地 HTTP 服务：每个连接读掉请求后回固定响应，供工具打真请求。
    fn spawn_server(responses: usize) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..responses {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\n\r\nok");
                let _ = stream.flush();
            }
        });
        port
    }

    #[tokio::test]
    async fn single_request_returns_status_and_body() {
        let port = spawn_server(1);
        let tool = ApiRequest::new();
        let text = ok_text(
            tool.call(json!({"method": "POST", "url": format!("http://127.0.0.1:{port}/api/x"), "body": "{\"a\":1}", "headers": {"Content-Type": "application/json"}}))
                .await
                .unwrap(),
        );
        assert!(text.contains("HTTP 200"), "got: {text}");
        assert!(text.contains("content-type: text/plain"));
        assert!(text.contains("ok"));
    }

    #[tokio::test]
    async fn concurrency_mode_reports_stats() {
        let total = 20;
        let port = spawn_server(total);
        let tool = ApiRequest::new();
        let text = ok_text(
            tool.call(json!({"url": format!("http://127.0.0.1:{port}/"), "concurrency": 5, "total": total}))
                .await
                .unwrap(),
        );
        assert!(text.contains(&format!("total={total}")), "got: {text}");
        assert!(text.contains("成功（2xx/3xx）20"), "got: {text}");
        assert!(text.contains("失败 0"), "got: {text}");
        assert!(text.contains("200×20"), "got: {text}");
        assert!(text.contains("p50=") && text.contains("p95="), "got: {text}");
    }

    #[tokio::test]
    async fn missing_url_is_rejected() {
        let tool = ApiRequest::new();
        let out = tool.call(json!({"method": "GET"})).await.unwrap();
        assert_eq!(out.state, ToolResultState::Error);
        assert!(out.content.contains("url 不能为空"), "got: {}", out.content);
    }
}
