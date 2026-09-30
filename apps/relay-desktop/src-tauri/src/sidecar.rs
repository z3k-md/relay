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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
}

impl ShellKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "zsh" => Some(Self::Zsh),
            "bash" => Some(Self::Bash),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    #[cfg(not(windows))]
    fn from_shell_path(path: &str) -> Option<Self> {
        let name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path);
        Self::parse(name)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliShellHint {
    pub shell: ShellKind,
    pub config_file: String,
    pub snippet: String,
    pub hint: String,
    pub configured: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliStatus {
    pub sidecar_path: Option<String>,
    pub install_path: Option<String>,
    pub on_path: bool,
    pub hint: Option<String>,
    pub detected_shell: Option<ShellKind>,
    pub shell_hints: Vec<CliShellHint>,
    pub path_configured: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliInstallResult {
    pub path: String,
    pub on_path: bool,
    pub hint: Option<String>,
    pub message: String,
    pub detected_shell: Option<ShellKind>,
    pub path_configured: bool,
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
            detected_shell: None,
            shell_hints: Vec::new(),
            path_configured: on_path,
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
        let detected_shell = detect_login_shell();
        let shell_hints = all_shell_hints();
        let path_configured = shell_config_has_path(detected_shell);
        let hint = if on_path && install_exists {
            None
        } else if !on_path && path_configured {
            Some(
                "PATH is configured in your shell config. Open a new terminal so it picks up the change."
                    .to_owned(),
            )
        } else if !on_path {
            Some(shell_hint_for(detected_shell).hint)
        } else {
            Some("The command-line tool is not installed in ~/.local/bin yet.".to_owned())
        };
        CliStatus {
            sidecar_path,
            install_path: install_exists.then(|| install.display().to_string()),
            on_path: on_path && install_exists,
            hint,
            detected_shell: Some(detected_shell),
            shell_hints,
            path_configured,
        }
    }
}

/// Install the CLI symlink/binary. When `configure_path` is true (user-initiated),
/// also append a PATH line to the chosen shell's config if needed.
pub fn install_cli(
    shell: Option<ShellKind>,
    configure_path: bool,
) -> anyhow::Result<CliInstallResult> {
    let sidecar = sidecar_path();
    if !sidecar.is_file() {
        anyhow::bail!(
            "bundled CLI not found at {} (or on PATH); run scripts/prepare-sidecar.sh and rebuild",
            sidecar.display()
        );
    }

    #[cfg(windows)]
    {
        let _ = shell;
        let _ = configure_path;
        windows_install(&sidecar)
    }

    #[cfg(not(windows))]
    {
        unix_install(&sidecar, shell, configure_path)
    }
}

#[cfg(not(windows))]
fn unix_install_link() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| "~".into());
    PathBuf::from(home).join(".local").join("bin").join("relay")
}

#[cfg(not(windows))]
fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "~".into()))
}

#[cfg(not(windows))]
pub(crate) fn shell_config_path(shell: ShellKind, home: &Path) -> PathBuf {
    match shell {
        ShellKind::Zsh => home.join(".zshrc"),
        ShellKind::Bash => home.join(".bashrc"),
        ShellKind::Fish => home.join(".config").join("fish").join("config.fish"),
    }
}

#[cfg(not(windows))]
pub(crate) fn shell_snippet(shell: ShellKind) -> &'static str {
    match shell {
        ShellKind::Zsh | ShellKind::Bash => r#"export PATH="$HOME/.local/bin:$PATH""#,
        ShellKind::Fish => r#"fish_add_path $HOME/.local/bin"#,
    }
}

#[cfg(not(windows))]
fn shell_config_display(shell: ShellKind) -> &'static str {
    match shell {
        ShellKind::Zsh => "~/.zshrc",
        ShellKind::Bash => "~/.bashrc",
        ShellKind::Fish => "~/.config/fish/config.fish",
    }
}

#[cfg(not(windows))]
fn shell_hint_for(shell: ShellKind) -> CliShellHint {
    let config_file = shell_config_display(shell).to_owned();
    let snippet = shell_snippet(shell).to_owned();
    let configured = shell_config_has_path(shell);
    let hint = if configured {
        format!(
            "PATH is already set in {config_file}. Open a new terminal so it picks up the change."
        )
    } else {
        match shell {
            ShellKind::Zsh => format!(
                "Add this line to ~/.zshrc (or ~/.zprofile): {snippet} — then open a new terminal."
            ),
            ShellKind::Bash => {
                format!("Add this line to ~/.bashrc: {snippet} — then open a new terminal.")
            }
            ShellKind::Fish => format!(
                "Add this line to ~/.config/fish/config.fish: {snippet} — then open a new terminal."
            ),
        }
    };
    CliShellHint {
        shell,
        config_file,
        snippet,
        hint,
        configured,
    }
}

#[cfg(not(windows))]
fn all_shell_hints() -> Vec<CliShellHint> {
    [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish]
        .into_iter()
        .map(shell_hint_for)
        .collect()
}

/// Detect the user's login shell. Prefers Directory Services / passwd over
/// macOS's zsh default, and notices an active fish setup when that is clearer.
#[cfg(not(windows))]
pub(crate) fn detect_login_shell() -> ShellKind {
    let login = read_login_shell_path();
    let env_shell = std::env::var("SHELL").ok();

    let from_login = login.as_deref().and_then(ShellKind::from_shell_path);
    let from_env = env_shell.as_deref().and_then(ShellKind::from_shell_path);

    // Prefer an explicit fish login shell or $SHELL over the macOS zsh default.
    if from_login == Some(ShellKind::Fish) || from_env == Some(ShellKind::Fish) {
        return ShellKind::Fish;
    }

    if let Some(shell) = from_login.or(from_env) {
        return shell;
    }

    // No login shell / $SHELL available: notice an existing fish config.
    if fish_config_exists() {
        ShellKind::Fish
    } else {
        ShellKind::Zsh
    }
}

#[cfg(not(windows))]
fn fish_config_exists() -> bool {
    shell_config_path(ShellKind::Fish, &home_dir()).is_file()
}

#[cfg(not(windows))]
fn read_login_shell_path() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        if let Some(shell) = read_macos_user_shell() {
            return Some(shell);
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(shell) = read_getent_shell() {
            return Some(shell);
        }
    }

    std::env::var("SHELL").ok()
}

#[cfg(target_os = "macos")]
fn read_macos_user_shell() -> Option<String> {
    let user = std::env::var("USER").ok()?;
    let output = Command::new("dscl")
        .args([".", "-read", &format!("/Users/{user}"), "UserShell"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_dscl_user_shell(&text)
}

#[cfg(target_os = "macos")]
fn parse_dscl_user_shell(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("UserShell:") else {
            continue;
        };
        let shell = rest.trim();
        if !shell.is_empty() {
            return Some(shell.to_owned());
        }
    }
    None
}

#[cfg(all(unix, not(target_os = "macos")))]
fn read_getent_shell() -> Option<String> {
    let user = std::env::var("USER").ok()?;
    let output = Command::new("getent")
        .args(["passwd", &user])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // name:x:uid:gid:gecos:home:shell
    text.trim().split(':').nth(6).map(|s| s.to_owned())
}

#[cfg(not(windows))]
pub(crate) fn path_line_present(content: &str, shell: ShellKind) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return false;
        }
        match shell {
            ShellKind::Fish => trimmed.contains("fish_add_path") && trimmed.contains(".local/bin"),
            ShellKind::Zsh | ShellKind::Bash => {
                trimmed.contains(".local/bin")
                    && (trimmed.contains("PATH") || trimmed.contains("path"))
            }
        }
    })
}

#[cfg(not(windows))]
fn shell_config_has_path(shell: ShellKind) -> bool {
    let path = shell_config_path(shell, &home_dir());
    std::fs::read_to_string(path)
        .map(|content| path_line_present(&content, shell))
        .unwrap_or(false)
}

/// Append the PATH snippet to the shell config if it is not already present.
/// Creates parent directories (for fish) as needed.
#[cfg(not(windows))]
pub(crate) fn ensure_shell_path_config(shell: ShellKind, home: &Path) -> anyhow::Result<PathBuf> {
    let path = shell_config_path(shell, home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let existing = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };

    if path_line_present(&existing, shell) {
        return Ok(path);
    }

    let snippet = shell_snippet(shell);
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.is_empty() {
        next.push('\n');
    }
    next.push_str("# Added by Relay\n");
    next.push_str(snippet);
    next.push('\n');
    std::fs::write(&path, next)?;
    Ok(path)
}

#[cfg(not(windows))]
fn unix_install(
    sidecar: &Path,
    shell: Option<ShellKind>,
    configure_path: bool,
) -> anyhow::Result<CliInstallResult> {
    let link = unix_install_link();
    let dir = link.parent().expect("link has a parent");
    let already_installed = link.exists() || link.is_symlink();
    std::fs::create_dir_all(dir)?;
    if already_installed {
        std::fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(sidecar, &link)?;

    let shell = shell.unwrap_or_else(detect_login_shell);
    let on_path = dir_is_on_path(dir);
    let mut path_configured = shell_config_has_path(shell);
    let wrote_path = configure_path && !on_path && !path_configured;

    if wrote_path {
        ensure_shell_path_config(shell, &home_dir())?;
        path_configured = true;
    }

    let hint = if on_path {
        None
    } else if path_configured {
        Some(
            "PATH is configured in your shell config. Open a new terminal so it picks up the change."
                .to_owned(),
        )
    } else {
        Some(shell_hint_for(shell).hint)
    };

    let message = if on_path {
        format!("Installed the `relay` command at {}.", link.display())
    } else if path_configured && configure_path {
        if wrote_path && already_installed {
            format!(
                "Updated {}. Open a new terminal so it picks up the PATH change.",
                shell_config_display(shell)
            )
        } else if wrote_path {
            format!(
                "Installed the `relay` command at {}. Updated {}. Open a new terminal so it picks up the PATH change.",
                link.display(),
                shell_config_display(shell)
            )
        } else {
            format!(
                "PATH is already set in {}. Open a new terminal so it picks up the change.",
                shell_config_display(shell)
            )
        }
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
        detected_shell: Some(shell),
        path_configured,
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
        detected_shell: None,
        path_configured: true,
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

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn snippets_match_shell() {
        assert_eq!(
            shell_snippet(ShellKind::Zsh),
            r#"export PATH="$HOME/.local/bin:$PATH""#
        );
        assert_eq!(
            shell_snippet(ShellKind::Bash),
            r#"export PATH="$HOME/.local/bin:$PATH""#
        );
        assert_eq!(
            shell_snippet(ShellKind::Fish),
            r#"fish_add_path $HOME/.local/bin"#
        );
    }

    #[test]
    fn path_line_present_detects_existing_entries() {
        assert!(path_line_present(
            r#"export PATH="$HOME/.local/bin:$PATH""#,
            ShellKind::Zsh
        ));
        assert!(path_line_present(
            "fish_add_path $HOME/.local/bin\n",
            ShellKind::Fish
        ));
        assert!(!path_line_present(
            "# export PATH=\"$HOME/.local/bin:$PATH\"\n",
            ShellKind::Zsh
        ));
        assert!(!path_line_present("set -gx EDITOR vim\n", ShellKind::Fish));
    }

    #[test]
    fn ensure_shell_path_config_appends_once() {
        let dir = std::env::temp_dir().join(format!("relay-cli-path-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let written = ensure_shell_path_config(ShellKind::Fish, &dir).unwrap();
        assert_eq!(
            written,
            dir.join(".config").join("fish").join("config.fish")
        );
        let first = fs::read_to_string(&written).unwrap();
        assert!(first.contains("fish_add_path $HOME/.local/bin"));

        ensure_shell_path_config(ShellKind::Fish, &dir).unwrap();
        let second = fs::read_to_string(&written).unwrap();
        assert_eq!(
            second.matches("fish_add_path $HOME/.local/bin").count(),
            1,
            "should not duplicate the PATH line"
        );

        let bash = ensure_shell_path_config(ShellKind::Bash, &dir).unwrap();
        assert_eq!(bash, dir.join(".bashrc"));
        let bash_content = fs::read_to_string(&bash).unwrap();
        assert!(bash_content.contains(r#"export PATH="$HOME/.local/bin:$PATH""#));

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_dscl_user_shell_line() {
        assert_eq!(
            parse_dscl_user_shell("UserShell: /opt/homebrew/bin/fish\n"),
            Some("/opt/homebrew/bin/fish".to_owned())
        );
        assert_eq!(
            parse_dscl_user_shell("UserShell: /bin/zsh"),
            Some("/bin/zsh".to_owned())
        );
    }

    #[test]
    fn shell_kind_from_paths() {
        assert_eq!(
            ShellKind::from_shell_path("/opt/homebrew/bin/fish"),
            Some(ShellKind::Fish)
        );
        assert_eq!(ShellKind::from_shell_path("/bin/zsh"), Some(ShellKind::Zsh));
        assert_eq!(
            ShellKind::from_shell_path("/bin/bash"),
            Some(ShellKind::Bash)
        );
    }
}
