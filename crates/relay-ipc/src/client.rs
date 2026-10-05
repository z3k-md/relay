use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use interprocess::local_socket::{Stream, prelude::*};
use relay_core::remote::{RemoteCall, RemoteReply};
use relay_core::{ConfigApplied, ConfigChange};

use crate::IpcError;
use crate::endpoint::Endpoint;
use crate::protocol::{
    ActivityItem, EvictResult, FetchParams, FolderPairParams, FolderPairPlan, FolderPairResult,
    Hello, OpenRemoteParams, OpenedRemote, PROTOCOL_VERSION, PairJoinParams, PairJoinResult,
    PairStartParams, PairStartResult, PairStatus, QuickOpen, RemoteParams, Request, RescanParams,
    RescanResult, Response, Status, decode_line, encode_line,
};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

pub struct Client {
    stream: BufReader<Stream>,
    next_id: u64,
}

impl Client {
    /// Connect to the host for `home`.
    ///
    /// Returns `Ok(None)` when no host is listening (missing endpoint,
    /// connection refused, or timeout). Other I/O failures are `Err`.
    pub fn connect(home: &Path) -> Result<Option<Self>, IpcError> {
        let endpoint = Endpoint::from_home(home);
        match connect_timeout(&endpoint, CONNECT_TIMEOUT) {
            Ok(stream) => Ok(Some(Self {
                stream: BufReader::new(stream),
                next_id: 1,
            })),
            Err(err) if is_no_host(&err) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub fn hello(&mut self) -> Result<Hello, IpcError> {
        let hello: Hello = self.call("hello", serde_json::json!({}))?;
        if hello.protocol != PROTOCOL_VERSION {
            return Err(IpcError::ProtocolMismatch {
                found: hello.protocol,
                expected: PROTOCOL_VERSION,
            });
        }
        Ok(hello)
    }

    pub fn status(&mut self) -> Result<Status, IpcError> {
        self.call("status", serde_json::json!({}))
    }

    pub fn pause(&mut self) -> Result<Status, IpcError> {
        self.call("pause", serde_json::json!({}))
    }

    pub fn resume(&mut self) -> Result<Status, IpcError> {
        self.call("resume", serde_json::json!({}))
    }

    pub fn fetch(&mut self, space: &str, mount: &str, path: &str) -> Result<(), IpcError> {
        let params = FetchParams {
            space: space.to_owned(),
            mount: mount.to_owned(),
            path: path.to_owned(),
        };
        let _: serde_json::Value = self.call(
            "fetch",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )?;
        Ok(())
    }

    /// Drop this device's copy of a demand-mode file, or of every downloaded
    /// one under a folder (`""` is the whole mount).
    pub fn evict(&mut self, space: &str, mount: &str, path: &str) -> Result<usize, IpcError> {
        let params = FetchParams {
            space: space.to_owned(),
            mount: mount.to_owned(),
            path: path.to_owned(),
        };
        let result: EvictResult = self.call(
            "evict",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )?;
        Ok(result.evicted)
    }

    pub fn rescan(
        &mut self,
        space: Option<&str>,
        mount: Option<&str>,
    ) -> Result<RescanResult, IpcError> {
        let params = RescanParams {
            space: space.map(ToOwned::to_owned),
            mount: mount.map(ToOwned::to_owned),
        };
        self.call(
            "rescan",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )
    }

    /// Apply a config change on the running host.
    pub fn config(&mut self, change: &ConfigChange) -> Result<ConfigApplied, IpcError> {
        self.call(
            "config",
            serde_json::to_value(change).map_err(IpcError::codec)?,
        )
    }

    pub fn pair_start(&mut self, params: &PairStartParams) -> Result<PairStartResult, IpcError> {
        self.call(
            "pair_start",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )
    }

    pub fn pair_status(&mut self) -> Result<PairStatus, IpcError> {
        self.call("pair_status", serde_json::json!({}))
    }

    pub fn pair_join(&mut self, params: &PairJoinParams) -> Result<PairJoinResult, IpcError> {
        self.call(
            "pair_join",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )
    }

    /// Open a file on a paired device: sync its folder here online-only if
    /// needed, download it, and return where it is.
    pub fn open_remote(&mut self, params: &OpenRemoteParams) -> Result<OpenedRemote, IpcError> {
        self.call(
            "open_remote",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )
    }

    pub fn quick_opens(&mut self) -> Result<Vec<QuickOpen>, IpcError> {
        self.call("quick_opens", serde_json::json!({}))
    }

    /// Undo a quick-open folder. Returns a note when the other device could
    /// not be reached.
    pub fn quick_open_remove(&mut self, space: &str) -> Result<Option<String>, IpcError> {
        let reply: serde_json::Value =
            self.call("quick_open_remove", serde_json::json!({ "space": space }))?;
        Ok(reply
            .get("note")
            .and_then(|v| v.as_str())
            .map(str::to_owned))
    }

    /// Check what a folder pair would do. Changes nothing.
    pub fn folder_pair_preview(
        &mut self,
        params: &FolderPairParams,
    ) -> Result<FolderPairPlan, IpcError> {
        self.call(
            "folder_pair_preview",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )
    }

    /// Set up a folder pair across devices. Undoes its own steps on failure.
    pub fn folder_pair(&mut self, params: &FolderPairParams) -> Result<FolderPairResult, IpcError> {
        self.call(
            "folder_pair",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )
    }

    /// Make a remote call on a paired device (D37). A refusal comes back as
    /// [`IpcError::Remote`] whose `code` is a `RemoteErrorCode` string.
    pub fn remote(&mut self, peer: &str, call: &RemoteCall) -> Result<RemoteReply, IpcError> {
        let params = RemoteParams {
            peer: peer.to_owned(),
            call: call.clone(),
        };
        self.call(
            "remote",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )
    }

    pub fn pair_cancel(&mut self) -> Result<(), IpcError> {
        let _: serde_json::Value = self.call("pair_cancel", serde_json::json!({}))?;
        Ok(())
    }

    pub fn activity(&mut self, limit: Option<u32>) -> Result<Vec<ActivityItem>, IpcError> {
        let params = match limit {
            Some(n) => serde_json::json!({"limit": n}),
            None => serde_json::json!({}),
        };
        self.call("activity", params)
    }

    /// Turn this connection into a live activity stream until disconnect.
    pub fn subscribe(mut self) -> Result<Subscribe, IpcError> {
        let _: serde_json::Value = self.call("subscribe", serde_json::json!({}))?;
        Ok(Subscribe {
            reader: self.stream,
        })
    }

    fn call<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T, IpcError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request {
            id,
            method: method.to_owned(),
            params,
        };
        let line = encode_line(&req).map_err(IpcError::codec)?;
        writeln!(self.stream.get_mut(), "{line}")?;
        self.stream.get_mut().flush()?;

        let mut buf = String::new();
        let n = self.stream.read_line(&mut buf)?;
        if n == 0 {
            return Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "host closed the connection",
            )));
        }
        let resp: Response = decode_line(&buf).map_err(IpcError::codec)?;
        if resp.id != id {
            return Err(IpcError::codec(format!(
                "response id {} does not match request {id}",
                resp.id
            )));
        }
        if let Some(err) = resp.error {
            return Err(IpcError::from_rpc(err));
        }
        let value = resp.result.unwrap_or(serde_json::Value::Null);
        serde_json::from_value(value).map_err(IpcError::codec)
    }
}

pub struct Subscribe {
    reader: BufReader<Stream>,
}

impl Subscribe {
    pub fn next_item(&mut self) -> Result<Option<ActivityItem>, IpcError> {
        let mut buf = String::new();
        let n = self.reader.read_line(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(decode_line(&buf).map_err(IpcError::codec)?))
    }
}

fn is_no_host(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::AddrNotAvailable
    )
}

fn connect_timeout(endpoint: &Endpoint, timeout: Duration) -> std::io::Result<Stream> {
    let endpoint = endpoint.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(connect_now(&endpoint));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out connecting to the Relay host",
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::other(
            "connect thread ended without a result",
        )),
    }
}

#[cfg(unix)]
pub(crate) fn connect_now(endpoint: &Endpoint) -> std::io::Result<Stream> {
    match endpoint {
        Endpoint::SocketFile(path) => {
            use interprocess::local_socket::{GenericFilePath, ToFsName};
            let name = path.clone().to_fs_name::<GenericFilePath>()?;
            Stream::connect(name)
        }
        Endpoint::NamedPipe(_) => Err(std::io::Error::other(
            "namespaced pipes are not used on this platform",
        )),
    }
}

#[cfg(windows)]
pub(crate) fn connect_now(endpoint: &Endpoint) -> std::io::Result<Stream> {
    match endpoint {
        Endpoint::NamedPipe(name) => {
            use interprocess::local_socket::{GenericNamespaced, ToNsName};
            let name = name.as_str().to_ns_name::<GenericNamespaced>()?;
            Stream::connect(name)
        }
        Endpoint::SocketFile(_) => Err(std::io::Error::other(
            "filesystem sockets are not used on this platform",
        )),
    }
}
