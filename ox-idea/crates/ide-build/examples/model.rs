//! Prints the build model of a folder, optionally after a sync:
//! `cargo run -p ide-build --example model -- <dir> [cache-dir] [maven|gradle]`.

fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::path::PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let cache =
        args.next().map(std::path::PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("ide-build-model"));
    let cancel = ide_core::CancellationToken::new();
    let progress = |p: ide_build::SyncProgress| eprintln!("  {} {:?}", p.message, p.fraction);
    let options = ide_build::SyncOptions::default();
    let started = std::time::Instant::now();
    let synced = match args.next().as_deref() {
        Some("maven") => Some(ide_build::sync_maven(&root, &cache, &options, &cancel, &progress)),
        Some("gradle") => Some(ide_build::sync_gradle(&root, &cache, &options, &cancel, &progress)),
        _ => None,
    };
    if let Some(r) = synced {
        eprintln!("synced in {:?}: {r:?}", started.elapsed());
    }
    let started = std::time::Instant::now();
    let model = ide_build::load(&root, &cache, None);
    println!("{}", serde_json::to_string_pretty(&model).unwrap());
    eprintln!("loaded in {:?}", started.elapsed());
}
