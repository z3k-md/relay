use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=RELAY_GIT_REV");
    emit_git_rerun();

    if let Ok(rev) = env::var("RELAY_GIT_REV") {
        println!("cargo:rustc-env=RELAY_GIT_REV={rev}");
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rustc-env=RELAY_GIT_REV={}", git_rev(&manifest_dir));
}

fn emit_git_rerun() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(&manifest_dir)
        .output();
    let Ok(output) = output else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let git_dir = String::from_utf8_lossy(&output.stdout);
    let git_dir = git_dir.trim();
    if git_dir.is_empty() {
        return;
    }
    let git_path = {
        let p = PathBuf::from(git_dir);
        if p.is_absolute() {
            p
        } else {
            manifest_dir.join(p)
        }
    };
    println!("cargo:rerun-if-changed={}", git_path.join("HEAD").display());
    println!(
        "cargo:rerun-if-changed={}",
        git_path.join("index").display()
    );
    println!("cargo:rerun-if-changed={}", git_path.join("refs").display());
}

fn git_rev(dir: &Path) -> String {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .current_dir(dir)
        .output();
    let Ok(sha) = sha else {
        return "unknown".to_owned();
    };
    if !sha.status.success() {
        return "unknown".to_owned();
    }
    let sha = String::from_utf8_lossy(&sha.stdout).trim().to_owned();
    if sha.is_empty() {
        return "unknown".to_owned();
    }

    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(dir)
        .output();
    match status {
        Ok(out)
            if out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty() =>
        {
            format!("{sha}+dirty")
        }
        _ => sha,
    }
}
