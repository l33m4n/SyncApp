//! Developer benchmark: back up a folder to a target folder and time it.
//!
//!   cargo run --release --example bench -- <source folder> <target folder> [background]
//!
//! Runs the same engine the app uses. Pass "background" to use the low I/O priority
//! that automatic backups use.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use syncapp::config::Source;
use syncapp::engine::{self, PcIdentity};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: bench <source> <target> [background]");
        std::process::exit(2);
    }
    let src = PathBuf::from(&args[1]);
    let target = PathBuf::from(&args[2]);
    if args.get(3).map(|s| s == "background").unwrap_or(false) {
        syncapp::win::thread_background_mode();
    }
    std::fs::create_dir_all(&target).unwrap();
    let sources = vec![Source { name: "Data".into(), path: src, exclude: vec![], enabled: true, kind: None }];
    let pc = PcIdentity { id: "bench".into(), name: "BENCH".into() };
    let no = AtomicBool::new(false);
    for run in ["first (everything new)", "second (nothing changed)"] {
        let t = Instant::now();
        let r = engine::run_backup(&sources, &target, &pc, &no, None);
        let secs = t.elapsed().as_secs_f64();
        println!(
            "{run}: {} copied, {} unchanged, {:.1} MB in {:.2}s = {:.0} MB/s, {:.1} ms/file, failed {}",
            r.copied.len(),
            r.unchanged,
            r.bytes_copied as f64 / 1e6,
            secs,
            r.bytes_copied as f64 / 1e6 / secs.max(0.001),
            secs * 1000.0 / (r.copied.len().max(1) as f64),
            r.failed.len()
        );
    }
}
