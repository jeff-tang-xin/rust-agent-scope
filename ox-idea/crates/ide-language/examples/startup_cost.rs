//! `cargo run --release -p ide-language --example startup_cost`: cost of the
//! language registry and syntax set-up done at app start.

use ide_language::{LanguageRegistry, Syntax};
use std::time::Instant;

fn main() {
    let t = Instant::now();
    let reg = LanguageRegistry::load(None).expect("registry");
    let registry = t.elapsed();
    let t = Instant::now();
    let syntax = Syntax::with_languages(&reg.all());
    let with_languages = t.elapsed();
    let t = Instant::now();
    let _ = Syntax::new();
    let new = t.elapsed();
    let t = Instant::now();
    let fp = syntax.fingerprint();
    let fingerprint = t.elapsed();
    println!(
        "LanguageRegistry::load {registry:?}\nSyntax::with_languages {with_languages:?}\nSyntax::new {new:?}\nfingerprint {fingerprint:?} ({fp})"
    );
}
