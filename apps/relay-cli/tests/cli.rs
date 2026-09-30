use std::fs;
use std::process::{Command as StdCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn relay() -> Command {
    Command::cargo_bin("relay").unwrap()
}

fn home_arg(home: &TempDir) -> String {
    home.path().to_str().unwrap().to_owned()
}

#[test]
fn cli_end_to_end() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    let mount_s = mount.path().to_str().unwrap().to_owned();

    fs::write(mount.path().join("a.txt"), b"hello").unwrap();

    relay()
        .args(["--home", &home_s, "init", "--name", "cli-dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("initialized device cli-dev"));

    relay()
        .args(["--home", &home_s, "space", "create", "Personal"])
        .assert()
        .success();

    relay()
        .args([
            "--home", &home_s, "mount", "add", "Personal", "code", &mount_s,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("added mount Personal/code"));

    relay()
        .args(["--home", &home_s, "scan", "Personal/code"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created"));

    let ls = relay()
        .args(["--home", &home_s, "--json", "ls", "Personal/code"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value = serde_json::from_slice(&ls).unwrap();
    assert!(parsed.is_array(), "{parsed}");
    assert!(
        parsed
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["key"]["path"] == "a.txt"),
        "{parsed}"
    );

    relay()
        .args(["--home", &home_s, "ls", "Personal/code/"])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt"));

    fs::write(mount.path().join("a.txt"), b"hello-2").unwrap();
    relay()
        .args(["--home", &home_s, "scan"])
        .assert()
        .success()
        .stdout(predicate::str::contains("modified"));

    relay()
        .args(["--home", &home_s, "history", "Personal/code/a.txt"])
        .assert()
        .success()
        .stdout(predicate::str::contains("file"));

    relay()
        .args([
            "--home",
            &home_s,
            "restore",
            "Personal/code/a.txt",
            "--sequence",
            "1",
        ])
        .assert()
        .success();
    assert_eq!(fs::read(mount.path().join("a.txt")).unwrap(), b"hello");

    relay()
        .args(["--home", &home_s, "verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("checked"));

    relay()
        .args(["--home", &home_s, "gc", "--grace-secs", "0"])
        .assert()
        .success();

    relay()
        .args(["--home", &home_s, "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cli-dev"))
        .stdout(predicate::str::contains("OK"))
        .stdout(predicate::str::contains("T"))
        .stdout(predicate::str::contains("Z"));

    relay()
        .args(["--home", &home_s, "mount", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("include:"))
        .stdout(predicate::str::contains("exclude:"));
}

#[test]
fn verify_corrupt_object_exits_three() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    let mount_s = mount.path().to_str().unwrap().to_owned();

    fs::write(mount.path().join("a.txt"), b"hello").unwrap();
    relay()
        .args(["--home", &home_s, "init", "--name", "cli-dev"])
        .assert()
        .success();
    relay()
        .args(["--home", &home_s, "space", "create", "Personal"])
        .assert()
        .success();
    relay()
        .args([
            "--home", &home_s, "mount", "add", "Personal", "code", &mount_s,
        ])
        .assert()
        .success();
    relay().args(["--home", &home_s, "scan"]).assert().success();

    let store = relay_engine::ObjectStore::open(home.path().join("store")).unwrap();
    let id = relay_engine::ObjectId::of(b"hello");
    fs::write(store.path_for(&id), b"corrupted").unwrap();

    relay()
        .args(["--home", &home_s, "verify"])
        .assert()
        .failure()
        .code(3)
        .stdout(predicate::str::contains("corrupt"));
}

#[test]
fn mass_delete_exits_two() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    let mount_s = mount.path().to_str().unwrap().to_owned();

    relay()
        .args(["--home", &home_s, "init", "--name", "cli-dev"])
        .assert()
        .success();
    relay()
        .args(["--home", &home_s, "space", "create", "Personal"])
        .assert()
        .success();
    relay()
        .args([
            "--home", &home_s, "mount", "add", "Personal", "code", &mount_s,
        ])
        .assert()
        .success();

    for i in 0..30 {
        fs::write(mount.path().join(format!("f{i}.txt")), b"x").unwrap();
    }
    relay().args(["--home", &home_s, "scan"]).assert().success();

    for i in 0..30 {
        fs::remove_file(mount.path().join(format!("f{i}.txt"))).unwrap();
    }
    relay()
        .args(["--home", &home_s, "scan"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("error:"))
        .stderr(predicate::str::contains("--allow-mass-delete"));
}

#[test]
fn missing_marker_is_clear_error() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    let mount_s = mount.path().to_str().unwrap().to_owned();

    fs::write(mount.path().join("a.txt"), b"x").unwrap();
    relay()
        .args(["--home", &home_s, "init", "--name", "cli-dev"])
        .assert()
        .success();
    relay()
        .args(["--home", &home_s, "space", "create", "Personal"])
        .assert()
        .success();
    relay()
        .args([
            "--home", &home_s, "mount", "add", "Personal", "code", &mount_s,
        ])
        .assert()
        .success();
    relay().args(["--home", &home_s, "scan"]).assert().success();

    fs::remove_file(mount.path().join(".relay-mount")).unwrap();
    relay()
        .args(["--home", &home_s, "scan"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("error:"))
        .stderr(predicate::str::contains("drive mounted"));
}

#[test]
fn dry_run_prefixes_report_and_writes_nothing() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    let mount_s = mount.path().to_str().unwrap().to_owned();
    fs::write(mount.path().join("a.txt"), b"hello").unwrap();

    relay()
        .args(["--home", &home_s, "init", "--name", "cli-dev"])
        .assert()
        .success();
    relay()
        .args(["--home", &home_s, "space", "create", "Personal"])
        .assert()
        .success();
    relay()
        .args([
            "--home", &home_s, "mount", "add", "Personal", "code", &mount_s,
        ])
        .assert()
        .success();

    relay()
        .args(["--home", &home_s, "scan", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(dry run)"))
        .stdout(predicate::str::contains("created"));

    let ls = relay()
        .args(["--home", &home_s, "--json", "ls", "Personal/code"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value = serde_json::from_slice(&ls).unwrap();
    assert_eq!(parsed.as_array().map(Vec::len), Some(0), "{parsed}");
}

#[test]
fn second_writer_reports_busy() {
    let home = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    relay()
        .args(["--home", &home_s, "init", "--name", "cli-dev"])
        .assert()
        .success();

    let _engine = relay_engine::Engine::open(home.path()).unwrap();
    relay()
        .args(["--home", &home_s, "space", "create", "Personal"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("another relay process is using"))
        .stderr(predicate::str::contains("relay watch"));
}

#[test]
fn mount_list_prints_rules() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    let mount_s = mount.path().to_str().unwrap().to_owned();

    relay()
        .args(["--home", &home_s, "init", "--name", "cli-dev"])
        .assert()
        .success();
    relay()
        .args(["--home", &home_s, "space", "create", "Personal"])
        .assert()
        .success();
    relay()
        .args([
            "--home",
            &home_s,
            "mount",
            "add",
            "Personal",
            "code",
            &mount_s,
            "--include",
            "src/**",
            "--exclude",
            "target/**",
        ])
        .assert()
        .success();

    relay()
        .args(["--home", &home_s, "mount", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("include: src/**"))
        .stdout(predicate::str::contains("exclude: target/**"));
}

#[test]
fn watch_indexes_new_file_then_stops_on_signal() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    let mount_s = mount.path().to_str().unwrap().to_owned();

    relay()
        .args(["--home", &home_s, "init", "--name", "cli-dev"])
        .assert()
        .success();
    relay()
        .args(["--home", &home_s, "space", "create", "Personal"])
        .assert()
        .success();
    relay()
        .args([
            "--home", &home_s, "mount", "add", "Personal", "code", &mount_s,
        ])
        .assert()
        .success();

    let bin = assert_cmd::cargo::cargo_bin("relay");
    let mut child = StdCommand::new(&bin)
        .args(["--home", &home_s, "watch", "--debounce-ms", "50"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let started = wait_until(Duration::from_secs(10), || {
        relay()
            .args(["--home", &home_s, "status"])
            .ok()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("Personal"))
    });
    assert!(started, "watch did not come up");

    fs::write(mount.path().join("seen.txt"), b"hi").unwrap();
    let appeared = wait_until(Duration::from_secs(10), || {
        relay()
            .args(["--home", &home_s, "--json", "ls", "Personal/code"])
            .ok()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("seen.txt"))
    });
    assert!(appeared, "watch did not index seen.txt");

    #[cfg(unix)]
    {
        let pid = child.id().to_string();
        let status = StdCommand::new("kill")
            .args(["-INT", &pid])
            .status()
            .unwrap();
        assert!(status.success(), "kill -INT failed");
        let exit = child.wait().unwrap();
        assert!(exit.success(), "watch exit {exit}");
    }
    #[cfg(not(unix))]
    {
        child.kill().unwrap();
        let _ = child.wait();
    }

    relay()
        .args(["--home", &home_s, "status"])
        .assert()
        .success();
}

#[test]
fn cli_identity_peers_share_and_conflicts() {
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let a = home_arg(&home_a);
    let b = home_arg(&home_b);

    relay()
        .args(["--home", &a, "init", "--name", "alpha"])
        .assert()
        .success();
    relay()
        .args(["--home", &b, "init", "--name", "bravo"])
        .assert()
        .success();

    let id_out = relay()
        .args(["--home", &a, "--json", "id"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id_json: serde_json::Value = serde_json::from_slice(&id_out).unwrap();
    let a_id = id_json["id"].as_str().unwrap().to_owned();
    let b_id = {
        let out = relay()
            .args(["--home", &b, "--json", "id"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    relay()
        .args([
            "--home",
            &a,
            "peer",
            "add",
            "bravo",
            &b_id,
            "--addr",
            "127.0.0.1:47321",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("added peer bravo"));
    relay()
        .args(["--home", &b, "peer", "add", "alpha", &a_id])
        .assert()
        .success();

    relay()
        .args(["--home", &a, "space", "create", "Personal"])
        .assert()
        .success();
    relay()
        .args(["--home", &a, "share", "Personal", "bravo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("shared Personal with bravo"));

    let peers = relay()
        .args(["--home", &a, "--json", "peer", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let peers: serde_json::Value = serde_json::from_slice(&peers).unwrap();
    assert_eq!(peers[0]["name"], "bravo");

    relay()
        .args(["--home", &a, "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bravo"));

    relay()
        .args(["--home", &a, "conflicts"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no conflicts"));

    relay()
        .args(["--home", &a, "space", "offers"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no offers"));

    relay()
        .args(["--home", &a, "unshare", "Personal", "bravo"])
        .assert()
        .success();
    relay()
        .args(["--home", &a, "peer", "remove", "bravo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed peer bravo"));
}

#[test]
fn version_includes_git_rev_parens() {
    relay()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("relay "))
        .stdout(predicate::str::contains("("));
}

#[test]
fn run_log_file_captures_listening_line() {
    let home = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    relay()
        .args(["--home", &home_s, "init", "--name", "cli-dev"])
        .assert()
        .success();

    let log_path = home.path().join("logs").join("relay.log");
    let log_s = log_path.to_str().unwrap().to_owned();
    let bin = assert_cmd::cargo::cargo_bin("relay");
    let mut child = StdCommand::new(&bin)
        .args([
            "--home",
            &home_s,
            "run",
            "--listen",
            "127.0.0.1:0",
            "--log-file",
            &log_s,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let found = wait_until(Duration::from_secs(15), || {
        fs::read_to_string(&log_path).is_ok_and(|s| s.contains("listening on"))
    });

    #[cfg(unix)]
    {
        let pid = child.id().to_string();
        let _ = StdCommand::new("kill").args(["-INT", &pid]).status();
        let _ = child.wait();
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
        let _ = child.wait();
    }

    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(found, "log file missing listening on:\n{log}");
    assert!(log.contains("==== relay "), "missing start header:\n{log}");
    assert!(
        log.contains("started "),
        "missing started timestamp:\n{log}"
    );
}

#[test]
fn service_logs_prints_tail() {
    let home = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    let log_dir = home.path().join("logs");
    fs::create_dir_all(&log_dir).unwrap();
    let mut body = String::new();
    for i in 1..=60 {
        body.push_str(&format!("line-{i}\n"));
    }
    fs::write(log_dir.join("relay.log"), body).unwrap();

    relay()
        .args(["--home", &home_s, "service", "logs", "-n", "3"])
        .assert()
        .success()
        .stdout(predicate::str::contains("line-58"))
        .stdout(predicate::str::contains("line-60"))
        .stdout(predicate::str::contains("line-1").not());
}

#[test]
fn service_status_errors_off_macos_windows() {
    if cfg!(any(target_os = "macos", windows)) {
        return;
    }
    let home = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    relay()
        .args(["--home", &home_s, "service", "status"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("supported on macOS and Windows"));
}

#[test]
fn status_json_daemon_null_without_host() {
    let home = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    relay()
        .args(["--home", &home_s, "init", "--name", "cli-dev"])
        .assert()
        .success();

    let out = relay()
        .args(["--home", &home_s, "--json", "status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(parsed["daemon"].is_null(), "{parsed}");
}

#[test]
fn offline_pause_and_resume_toggle_flag() {
    let home = TempDir::new().unwrap();
    let home_s = home_arg(&home);
    relay()
        .args(["--home", &home_s, "init", "--name", "cli-dev"])
        .assert()
        .success();

    relay()
        .args(["--home", &home_s, "pause"])
        .assert()
        .success()
        .stdout(predicate::str::contains("paused"));

    let engine = relay_engine::Engine::open_read_only(home.path()).unwrap();
    assert!(engine.paused().unwrap());

    relay()
        .args(["--home", &home_s, "resume"])
        .assert()
        .success()
        .stdout(predicate::str::contains("resume"));

    let engine = relay_engine::Engine::open_read_only(home.path()).unwrap();
    assert!(!engine.paused().unwrap());
}

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(40));
    }
}
