//! Prints the run configurations detected for a directory: `cargo run -p ide-run --example detect -- <dir>`.

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    for c in ide_run::detect(std::path::Path::new(&root)) {
        let cwd = c.cwd.as_ref().map(|p| format!(" (in {})", p.display())).unwrap_or_default();
        println!("{:<28} {} {}{cwd}", c.id, c.command, c.args.join(" "));
    }
}
