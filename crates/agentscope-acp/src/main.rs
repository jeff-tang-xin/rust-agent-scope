//! agentscope-acp 入口：stdio 上的 ACP v1 server。
//!
//! 环境变量：
//! - `AGENTSCOPE_PROVIDER`：dashscope（默认）/ openai / anthropic / gemini / ollama
//! - `AGENTSCOPE_API_KEY` / `AGENTSCOPE_MODEL` / `AGENTSCOPE_BASE_URL`
//! - `RUST_LOG`：日志过滤（默认 info；日志走 stderr，不污染 stdio 协议通道）

use tokio::io::stdin;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(error) = agentscope_acp::serve(stdin(), tokio::io::stdout()).await {
        tracing::error!(%error, "acp: 服务循环退出");
        std::process::exit(1);
    }
}
