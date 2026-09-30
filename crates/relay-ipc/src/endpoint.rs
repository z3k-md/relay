use std::path::{Path, PathBuf};

use crate::IpcError;

#[cfg(not(windows))]
const SOCK_NAME: &str = "relay.sock";
#[cfg(not(windows))]
const UNIX_PATH_LIMIT: usize = 100;

/// First 16 hex characters of BLAKE3(canonicalized home path).
pub fn home_token(home: &Path) -> String {
    let canon = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let hash = blake3::hash(canon.as_os_str().as_encoded_bytes());
    hex::encode(&hash.as_bytes()[..8])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// Filesystem Unix-domain socket.
    SocketFile(PathBuf),
    /// Windows namespaced named pipe (`relay-<token>`).
    NamedPipe(String),
}

impl Endpoint {
    pub fn from_home(home: &Path) -> Self {
        let token = home_token(home);
        #[cfg(windows)]
        {
            Self::NamedPipe(format!("relay-{token}"))
        }
        #[cfg(not(windows))]
        {
            let sock = home.join(SOCK_NAME);
            let bytes = sock.as_os_str().as_encoded_bytes();
            if bytes.len() > UNIX_PATH_LIMIT {
                Self::SocketFile(std::env::temp_dir().join(format!("relay-{token}.sock")))
            } else {
                Self::SocketFile(sock)
            }
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::SocketFile(path) => path.display().to_string(),
            Self::NamedPipe(name) => name.clone(),
        }
    }

    pub(crate) fn remove_stale(&self) -> Result<(), IpcError> {
        if let Self::SocketFile(path) = self
            && path.exists()
        {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn set_socket_mode(&self) -> Result<(), IpcError> {
        use std::os::unix::fs::PermissionsExt;
        if let Self::SocketFile(path) = self
            && path.exists()
        {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}
