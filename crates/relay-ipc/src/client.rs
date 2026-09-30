use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use interprocess::local_socket::{Stream, prelude::*};

use crate::IpcError;
use crate::endpoint::Endpoint;
use crate::protocol::{
    ActivityItem, AddMountParams, AddMountResult, Hello, PROTOCOL_VERSION, PairJoinParams,
    PairJoinResult, PairStartParams, PairStartResult, PairStatus, Request, RescanParams,
    RescanResult, Response, ShareParams, Status, decode_line, encode_line,
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

    pub fn add_mount(
        &mut self,
        space: &str,
        mount: &str,
        path: &Path,
    ) -> Result<AddMountResult, IpcError> {
        let params = AddMountParams {
            space: space.to_owned(),
            mount: mount.to_owned(),
            path: path.to_path_buf(),
        };
        self.call(
            "add_mount",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )
    }

    pub fn share(&mut self, space: &str, peer: &str) -> Result<(), IpcError> {
        let params = ShareParams {
            space: space.to_owned(),
            peer: peer.to_owned(),
        };
        let _: serde_json::Value = self.call(
            "share",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )?;
        Ok(())
    }

    pub fn pair_start(&mut self, share: &[String]) -> Result<PairStartResult, IpcError> {
        let params = PairStartParams {
            share: share.to_vec(),
        };
        self.call(
            "pair_start",
            serde_json::to_value(params).map_err(IpcError::codec)?,
        )
    }

    pub fn pair_status(&mut self) -> Result<PairStatus, IpcError> {
        self.call("pair_status", serde_json::json!({}))
    }

    pub fn pair_join(
        &mut self,
        code: &str,
        addr: Option<&str>,
    ) -> Result<PairJoinResult, IpcError> {
        let params = PairJoinParams {
            code: code.to_owned(),
            addr: addr.map(ToOwned::to_owned),
        };
        self.call(
            "pair_join",
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
fn connect_now(endpoint: &Endpoint) -> std::io::Result<Stream> {
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
fn connect_now(endpoint: &Endpoint) -> std::io::Result<Stream> {
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
