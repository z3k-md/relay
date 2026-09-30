//! End-to-end lab: real daemon processes, localhost QUIC, optional mailbox.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

struct Lab(tempfile::TempDir);

impl Lab {
    fn new() -> Self {
        Self(tempfile::tempdir().expect("temp lab"))
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = Command::new(bin())
            .arg("--lab")
            .arg(self.path())
            .arg("down")
            .output();
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_relay-sim")
}

fn run(lab: &Path, args: &[&str]) -> Output {
    let output = Command::new(bin())
        .arg("--lab")
        .arg(lab)
        .args(args)
        .output()
        .expect("spawn relay-sim");
    if !output.status.success() {
        fail(args, &output);
    }
    output
}

fn fail(args: &[&str], output: &Output) -> ! {
    panic!(
        "relay-sim {} failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn report(lab: &Path) -> Value {
    let output = run(lab, &["report"]);
    serde_json::from_slice(&output.stdout).expect("report json")
}

fn assert_file(report: &Value, path: &str, content: &str) {
    let nodes = report["nodes"].as_array().expect("nodes");
    assert!(!nodes.is_empty());
    for node in nodes {
        assert_eq!(
            node["tree"][path].as_str(),
            Some(content),
            "{} tree: {}",
            node["name"],
            node["tree"]
        );
        assert_eq!(node["running"], true, "{}", node["name"]);
    }
}

#[test]
fn pair_syncs_a_file_both_ways() {
    let lab = Lab::new();
    run(lab.path(), &["up", "mac", "pc"]);
    run(lab.path(), &["write", "mac", "mods/hello.txt", "from-a"]);
    run(lab.path(), &["wait-converged", "--timeout", "20s"]);
    assert_file(&report(lab.path()), "hello.txt", "from-a");

    run(lab.path(), &["write", "pc", "mods/hello.txt", "from-b"]);
    run(lab.path(), &["wait-converged", "--timeout", "20s"]);
    assert_file(&report(lab.path()), "hello.txt", "from-b");
}

#[test]
fn kill_during_mailbox_push_still_converges() {
    let lab = Lab::new();
    run(lab.path(), &["up", "mac", "pc", "--mailbox", "shared"]);
    run(lab.path(), &["write", "mac", "mods/weapon.esp", "v1"]);
    run(lab.path(), &["wait-converged", "--timeout", "20s"]);

    run(lab.path(), &["stall", "mac"]);
    run(lab.path(), &["write", "mac", "mods/weapon.esp", "v2"]);
    run(lab.path(), &["wait-stalled", "mac", "--timeout", "20s"]);
    run(lab.path(), &["kill", "mac"]);
    run(lab.path(), &["unstall", "mac"]);
    run(lab.path(), &["start", "mac"]);
    run(lab.path(), &["wait-converged", "--timeout", "30s"]);
    assert_file(&report(lab.path()), "weapon.esp", "v2");
}
