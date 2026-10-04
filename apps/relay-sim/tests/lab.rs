//! End-to-end lab: real daemon processes, localhost QUIC, optional mailbox.

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
        let _ = sim(self.path(), &["down"]);
    }
}

/// Longest any one `relay-sim` command may take. The slowest waits in these
/// tests are 30 s; anything past this is stuck.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_relay-sim")
}

fn run(lab: &Path, args: &[&str]) -> Output {
    let output = sim(lab, args).unwrap_or_else(|output| {
        fail(&[&["timed out:"], args].concat(), &output);
    });
    if !output.status.success() {
        fail(args, &output);
    }
    output
}

/// Run `relay-sim` with its output in files, not pipes, and a deadline.
///
/// On Windows a child inherits every inheritable handle its parent holds, so
/// the daemons `relay-sim up` starts keep the pipes `Command::output` would
/// make, and reading those pipes waits until the daemons exit, which they
/// never do on their own. Files have no reader to block. `Err` carries what
/// was printed before the deadline.
fn sim(lab: &Path, args: &[&str]) -> Result<Output, Output> {
    let dir = tempfile::tempdir().expect("output dir");
    let (out_path, err_path) = (dir.path().join("stdout"), dir.path().join("stderr"));
    let mut child = Command::new(bin())
        .arg("--lab")
        .arg(lab)
        .args(args)
        .stdin(Stdio::null())
        .stdout(fs::File::create(&out_path).expect("stdout file"))
        .stderr(fs::File::create(&err_path).expect("stderr file"))
        .spawn()
        .expect("spawn relay-sim");
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let timed_out = loop {
        if child.try_wait().expect("wait for relay-sim").is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break true;
        }
        thread::sleep(Duration::from_millis(50));
    };
    let output = Output {
        status: child.wait().expect("reap relay-sim"),
        stdout: fs::read(&out_path).unwrap_or_default(),
        stderr: fs::read(&err_path).unwrap_or_default(),
    };
    if timed_out { Err(output) } else { Ok(output) }
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
