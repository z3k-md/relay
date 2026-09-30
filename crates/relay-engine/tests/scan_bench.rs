use std::fs;
use std::time::Instant;

use relay_engine::{Engine, ScanOptions};
use tempfile::TempDir;

/// First-scan wall time for 2000 small files. Run with:
/// `cargo nextest run -p relay-engine --locked --run-ignored ignored-only first_scan_2000`
#[test]
#[ignore]
fn first_scan_2000_small_files_elapsed() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    for i in 0..2000 {
        fs::write(
            mount.path().join(format!("f{i:04}.txt")),
            format!("payload-{i}"),
        )
        .unwrap();
    }

    let mut engine = Engine::init(home.path(), "testdev").unwrap();
    engine.create_space("Personal").unwrap();
    engine
        .add_mount("Personal", "code", mount.path(), &[], &[])
        .unwrap();

    let start = Instant::now();
    let report = engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(report.created, 2000, "{report:?}");
    eprintln!("first_scan_2000_small_files: {elapsed:.3?}");
}
