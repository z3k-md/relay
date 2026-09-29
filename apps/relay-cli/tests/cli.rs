use std::fs;

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
