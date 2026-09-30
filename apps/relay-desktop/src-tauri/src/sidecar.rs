use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Resolve the bundled CLI next to the app executable (where Tauri places
/// `externalBin`), falling back to `relay` on PATH.
pub fn sidecar_path() -> PathBuf {
    let name = if cfg!(windows) { "relay.exe" } else { "relay" };
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(if cfg!(windows) { "relay.exe" } else { "relay" })
}

#[derive(Debug, Deserialize)]
struct ServiceStatusJson {
    running: Option<bool>,
}

pub fn service_is_running(home: &Path) -> bool {
    let bin = sidecar_path();
    let output = Command::new(&bin)
        .arg("--home")
        .arg(home)
        .args(["service", "status", "--json"])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    serde_json::from_slice::<ServiceStatusJson>(&output.stdout)
        .ok()
        .and_then(|s| s.running)
        .unwrap_or(false)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliStatus {
    pub sidecar_path: Option<String>,
    pub install_path: Option<String>,
    pub on_path: bool,
    pub hint: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliInstallResult {
    pub path: String,
    pub on_path: bool,
    pub hint: Option<String>,
    pub message: String,
}

pub fn cli_status() -> CliStatus {
    let sidecar = sidecar_path();
    let sidecar_exists = sidecar.is_file();
    let sidecar_path = sidecar_exists.then(|| sidecar.display().to_string());

    #[cfg(windows)]
    {
        let install = sidecar
            .parent()
            .map(|p| p.to_path_buf())
            .filter(|p| p.is_dir());
        let on_path = install
            .as_ref()
            .map(|dir| dir_is_on_path(dir))
            .unwrap_or(false);
        let hint = if sidecar_exists && !on_path {
            Some(
                "The Relay CLI folder is not on your user PATH. Click Install, then open a new terminal."
                    .to_owned(),
            )
        } else {
            None
        };
        CliStatus {
            sidecar_path,
            install_path: install.map(|p| p.display().to_string()),
            on_path,
            hint,
        }
    }

    #[cfg(not(windows))]
    {
        let install = unix_install_link();
        let install_exists = install.is_file() || install.is_symlink();
        let bin_dir = install.parent().map(|p| p.to_path_buf());
        let on_path = bin_dir
            .as_ref()
            .map(|dir| dir_is_on_path(dir))
            .unwrap_or(false);
        let hint = if !on_path {
            Some(unix_path_hint())
        } else if !install_exists {
            Some("The command-line tool is not installed in ~/.local/bin yet.".to_owned())
        } else {
            None
        };
        CliStatus {
            sidecar_path,
            install_path: install_exists.then(|| install.display().to_string()),
            on_path: on_path && install_exists,
            hint,
        }
    }
}

pub fn install_cli() -> anyhow::Result<CliInstallResult> {
    let sidecar = sidecar_path();
    if !sidecar.is_file() {
        anyhow::bail!(
            "bundled CLI not found at {} (or on PATH); run scripts/prepare-sidecar.sh and rebuild",
            sidecar.display()
        );
    }

    #[cfg(windows)]
    {
        windows_install(&sidecar)
    }

    #[cfg(not(windows))]
    {
        unix_install(&sidecar)
    }
}

#[cfg(not(windows))]
fn unix_install_link() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| "~".into());
    PathBuf::from(home).join(".local").join("bin").join("relay")
}

#[cfg(not(windows))]
fn unix_path_hint() -> String {
    r#"Add this line to ~/.zshrc (or ~/.zprofile): export PATH="$HOME/.local/bin:$PATH" — then open a new terminal."#
        .to_owned()
}

#[cfg(not(windows))]
fn unix_install(sidecar: &Path) -> anyhow::Result<CliInstallResult> {
    let link = unix_install_link();
    let dir = link.parent().expect("link has a parent");
    std::fs::create_dir_all(dir)?;
    if link.exists() || link.is_symlink() {
        std::fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(sidecar, &link)?;
    let on_path = dir_is_on_path(dir);
    let hint = if on_path {
        None
    } else {
        Some(unix_path_hint())
    };
    let message = if on_path {
        format!("Installed the `relay` command at {}.", link.display())
    } else {
        format!(
            "Installed the `relay` command at {}. ~/.local/bin is not on PATH yet.",
            link.display()
        )
    };
    Ok(CliInstallResult {
        path: link.display().to_string(),
        on_path,
        hint,
        message,
    })
}

#[cfg(windows)]
fn windows_install(sidecar: &Path) -> anyhow::Result<CliInstallResult> {
    let dir = sidecar
        .parent()
        .ok_or_else(|| anyhow::anyhow!("sidecar has no parent directory"))?;
    add_user_path(dir)?;
    let on_path = dir_is_on_path(dir);
    Ok(CliInstallResult {
        path: sidecar.display().to_string(),
        on_path,
        hint: Some(
            "Open a new terminal so it picks up the updated PATH. Existing terminals keep the old one."
                .to_owned(),
        ),
        message: format!(
            "Added {} to your user PATH so the `relay` command is available.",
            dir.display()
        ),
    })
}

#[cfg(windows)]
fn add_user_path(dir: &Path) -> anyhow::Result<()> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let env = hkcu.open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)?;
    let current: String = env.get_value("Path").unwrap_or_default();
    let dir_s = dir.to_string_lossy();
    if current
        .split(';')
        .any(|p| p.eq_ignore_ascii_case(dir_s.as_ref()))
    {
        return Ok(());
    }
    let new_path = if current.is_empty() {
        dir_s.into_owned()
    } else if current.ends_with(';') {
        format!("{current}{dir_s}")
    } else {
        format!("{current};{dir_s}")
    };
    env.set_value("Path", &new_path)?;
    Ok(())
}

fn dir_is_on_path(dir: &Path) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    let sep = if cfg!(windows) { ';' } else { ':' };
    path.split(sep).any(|entry| {
        let p = Path::new(entry);
        p == dir || paths_equal_loose(p, dir)
    })
}

fn paths_equal_loose(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}
