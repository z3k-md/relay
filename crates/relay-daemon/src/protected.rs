//! Folders a managing device never reaches (D45).
//!
//! A manage grant (D37) lets a peer browse this device and set up sync on
//! it. It stops at what holds credentials and private state: SSH and GPG
//! keys, cloud and Kubernetes credentials, password stores, the operating
//! system's keychains and keyrings, browser profiles, and the system's own
//! secret files. [`crate::remote`] leaves them out of listings, refuses them
//! when named, and lets no remote mount sit inside one or contain one. The
//! list is fixed and built from this user's folders; only what exists on
//! disk counts, and every entry is canonical, so a symlink into one is still
//! inside it.

use std::fs;
use std::path::{Path, PathBuf};

/// The folders the operating system gives this user.
pub(crate) struct UserDirs {
    pub(crate) home: PathBuf,
    /// `~/.config`, `~/Library/Application Support`, or `%APPDATA%`.
    pub(crate) config: PathBuf,
    /// `~/.local/share`, `~/Library/Application Support`, or `%LOCALAPPDATA%`.
    pub(crate) data_local: PathBuf,
}

impl UserDirs {
    fn current() -> Option<Self> {
        let base = directories::BaseDirs::new()?;
        Some(Self {
            home: base.home_dir().to_path_buf(),
            config: base.config_dir().to_path_buf(),
            data_local: base.data_local_dir().to_path_buf(),
        })
    }
}

/// Under the home folder, on every platform.
const IN_HOME: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".password-store",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".terraform.d/credentials.tfrc.json",
    ".config/gcloud",
    ".config/op",
];

/// Under the home folder, on this platform.
#[cfg(target_os = "macos")]
const IN_HOME_OS: &[&str] = &["Library/Keychains", "Library/Cookies"];
#[cfg(windows)]
const IN_HOME_OS: &[&str] = &[];
#[cfg(not(any(windows, target_os = "macos")))]
const IN_HOME_OS: &[&str] = &[".mozilla", ".thunderbird", ".pki"];

/// Under the config folder.
#[cfg(target_os = "macos")]
const IN_CONFIG: &[&str] = &[
    "Google/Chrome",
    "Chromium",
    "BraveSoftware",
    "Microsoft Edge",
    "Vivaldi",
    "Firefox",
    "Thunderbird",
    "1Password",
];
#[cfg(windows)]
const IN_CONFIG: &[&str] = &[
    "Microsoft\\Credentials",
    "Microsoft\\Protect",
    "Microsoft\\Crypto",
    "Mozilla\\Firefox",
    "Thunderbird",
    "gnupg",
    "gcloud",
];
#[cfg(not(any(windows, target_os = "macos")))]
const IN_CONFIG: &[&str] = &[
    "google-chrome",
    "chromium",
    "BraveSoftware",
    "microsoft-edge",
    "vivaldi",
];

/// Under the local data folder.
#[cfg(target_os = "macos")]
const IN_DATA_LOCAL: &[&str] = &[];
#[cfg(windows)]
const IN_DATA_LOCAL: &[&str] = &[
    "Microsoft\\Credentials",
    "Google\\Chrome\\User Data",
    "Chromium\\User Data",
    "Microsoft\\Edge\\User Data",
    "BraveSoftware\\Brave-Browser\\User Data",
    "Vivaldi\\User Data",
    "1Password",
];
#[cfg(not(any(windows, target_os = "macos")))]
const IN_DATA_LOCAL: &[&str] = &["keyrings", "kwalletd"];

/// Protected folders and files on this device, canonical.
pub(crate) fn paths() -> Vec<PathBuf> {
    let mut candidates = system_candidates();
    if let Some(dirs) = UserDirs::current() {
        candidates.extend(user_candidates(&dirs));
    }
    existing(&candidates)
}

/// Where one user's credential stores would be, whether or not they exist.
pub(crate) fn user_candidates(dirs: &UserDirs) -> Vec<PathBuf> {
    let under = |base: &Path, names: &[&str]| -> Vec<PathBuf> {
        names.iter().map(|name| base.join(name)).collect()
    };
    let mut out = under(&dirs.home, IN_HOME);
    out.extend(under(&dirs.home, IN_HOME_OS));
    out.extend(under(&dirs.config, IN_CONFIG));
    out.extend(under(&dirs.data_local, IN_DATA_LOCAL));
    out
}

/// Secret files outside any user's folders.
#[cfg(unix)]
fn system_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = [
        "/etc/shadow",
        "/etc/gshadow",
        "/etc/ssh",
        "/etc/ssl/private",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if cfg!(target_os = "macos") {
        out.push(PathBuf::from("/Library/Keychains"));
    }
    out
}

#[cfg(windows)]
fn system_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(root) = std::env::var_os("SystemRoot") {
        out.push(PathBuf::from(root).join("System32\\config"));
    }
    if let Some(data) = std::env::var_os("ProgramData") {
        out.push(PathBuf::from(data).join("ssh"));
    }
    out
}

/// Those of `candidates` that exist, canonical and without duplicates.
pub(crate) fn existing(candidates: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = candidates
        .iter()
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_what_exists_canonical() {
        let root = tempfile::TempDir::new().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(home.join(".netrc"), b"machine x login y").unwrap();
        let dirs = UserDirs {
            home: home.clone(),
            config: home.join(".config"),
            data_local: home.join(".local/share"),
        };
        let candidates = user_candidates(&dirs);
        assert!(
            candidates.contains(&home.join(".aws")),
            "every store is a candidate, present or not"
        );
        let canonical = dunce::canonicalize(&home).unwrap();
        assert_eq!(
            existing(&candidates),
            [canonical.join(".netrc"), canonical.join(".ssh")]
        );
    }
}
