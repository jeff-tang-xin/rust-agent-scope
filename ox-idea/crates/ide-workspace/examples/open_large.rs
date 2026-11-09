//! `cargo run --release -p ide-workspace --example open_large [MiB]`: time and
//! peak-memory cost of opening a large file as a document (default 100 MiB).

use ide_core::{LanguageId, NullSink};
use ide_workspace::Documents;
use ide_workspace::process::resident_bytes;
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let mib: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(100);
    let dir = std::env::temp_dir().join(format!("openidea-open-large-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("big.log");
    let line = "2026-10-20T12:00:00Z INFO request served path=/api/v1/items id=12345 é\n";
    let text = line.repeat(mib * 1024 * 1024 / line.len());
    std::fs::write(&path, &text).expect("write");
    drop(text);

    let docs =
        Documents::new(Arc::new(NullSink), Box::new(|_: &std::path::Path, _: Option<&str>| LanguageId::plain_text()));
    let rss0 = resident_bytes().unwrap_or(0);
    let t = Instant::now();
    let info = docs.open(&path).expect("open");
    let open = t.elapsed();
    let rss1 = resident_bytes().unwrap_or(0);
    let t = Instant::now();
    let json = serde_json::to_string(&info).expect("json");
    let ser = t.elapsed();
    println!(
        "{mib} MiB: open {open:?}, serialize for IPC {ser:?} ({} MiB JSON), large={}, read_only={}, rss +{} MiB after open",
        json.len() >> 20,
        info.large,
        info.read_only,
        (rss1.saturating_sub(rss0)) >> 20
    );
    let _ = std::fs::remove_dir_all(&dir);
}
