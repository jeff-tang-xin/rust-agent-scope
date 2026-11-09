//! ox-hareness 的 IDE 语义工具注入：提供者按任务目录挑项目、透传工具元数据，
//! 只读工具（`read_file`）经 spawn_blocking 真跑一次。

use agentscope_core::message::ToolResultState;
use ide_host::ox_tools::ox_tool_provider;
use ide_host::{Host, HostConfig, ops};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

const WINDOW: &str = "main";

struct Fixture {
    host: Arc<Host>,
    root: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    let root = dunce::canonicalize(tmp.path()).unwrap().join("rust-app");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"rust-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src").join("main.rs"), "fn main() {\n    println!(\"hello\");\n}\n").unwrap();
    let host = Arc::new(Host::new(HostConfig {
        config_dir: app.join("config"),
        data_dir: app.join("data"),
        local_data_dir: app.join("data"),
        languages: Arc::new(ide_language::LanguageRegistry::load(None).unwrap()),
        syntax: Arc::default(),
        sink_for: Box::new(|_| Arc::new(|_| {})),
        owner: |w| w,
        runtime: tokio::runtime::Handle::current(),
        project_environment: false,
    }));
    host.trust.decide(&root, true, false).unwrap();
    Fixture { host, root, _tmp: tmp }
}

#[tokio::test]
async fn provider_without_open_project_is_empty() {
    let f = fixture();
    let provider = ox_tool_provider(f.host);
    assert!(provider(Path::new("C:/nowhere")).tools.is_empty());
}

#[tokio::test]
async fn provider_serves_project_tools_and_calls_read_file() {
    let f = fixture();
    ops::project::open(&f.host, WINDOW, &f.root).await.unwrap();

    let provider = ox_tool_provider(f.host.clone());
    let tools = provider(&f.root).tools;
    assert!(!tools.is_empty(), "open project must expose its tools");

    // worktree 风格的子目录也要命中同一个项目。
    let nested = f.root.join("src").join("deep");
    assert!(!provider(&nested).tools.is_empty(), "subdirectory of the project must match");

    let read_file = tools.iter().find(|t| t.name() == "read_file").expect("read_file tool");
    assert!(read_file.is_read_only());
    assert!(read_file.parameters().is_object());

    let output = read_file.call(json!({ "file_path": "src/main.rs" })).await.unwrap();
    assert_eq!(output.state, ToolResultState::Success, "output: {}", output.content);
    assert!(output.content.contains("fn main"), "main.rs body missing: {}", output.content);
}

#[tokio::test]
async fn trusted_project_exposes_write_tools() {
    let f = fixture();
    ops::project::open(&f.host, WINDOW, &f.root).await.unwrap();

    let provider = ox_tool_provider(f.host);
    let tools = provider(&f.root).tools;
    let read_only = tools.iter().filter(|t| t.is_read_only()).count();
    assert!(read_only > 0);
    // 项目已受信任：写工具（如 apply_patch）也在列。
    assert!(tools.len() > read_only, "trusted project must also expose write tools");
}
