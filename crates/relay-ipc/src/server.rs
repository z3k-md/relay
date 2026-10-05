use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use interprocess::local_socket::{Listener, Stream, prelude::*};

use crate::IpcError;
use crate::endpoint::Endpoint;
use crate::protocol::{ActivityItem, Request, Response, RpcErrorBody, decode_line, encode_line};

pub trait Handler: Send + Sync + 'static {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcErrorBody>;
    fn subscribe(&self) -> mpsc::Receiver<ActivityItem>;
}

pub struct Server {
    listener: Listener,
    endpoint: Endpoint,
}

impl Server {
    pub fn bind(home: &Path) -> Result<Self, IpcError> {
        let endpoint = Endpoint::from_home(home);
        endpoint.remove_stale()?;
        let listener = bind_listener(&endpoint)?;
        #[cfg(unix)]
        endpoint.set_socket_mode()?;
        Ok(Self { listener, endpoint })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Accept clients until `stop` is set. One thread per connection.
    ///
    /// Accept blocks, so a client is served the moment it connects (the
    /// desktop app connects once per command and used to wait out a 50 ms
    /// poll each time). Once `stop` is set, a watcher wakes the accept with
    /// throwaway connections until the loop has ended.
    pub fn serve<H: Handler>(self, handler: Arc<H>, stop: &AtomicBool) {
        let Self { listener, endpoint } = self;
        let ended = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                while !ended.load(Ordering::Relaxed) {
                    if stop.load(Ordering::Relaxed) {
                        let _ = crate::client::connect_now(&endpoint);
                    }
                    thread::sleep(STOP_POLL);
                }
            });
            loop {
                match listener.accept() {
                    Ok(stream) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let handler = Arc::clone(&handler);
                        thread::spawn(move || {
                            let _ = handle_client(stream, handler.as_ref());
                        });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(STOP_POLL);
                    }
                }
            }
            ended.store(true, Ordering::Relaxed);
        });
    }
}

/// How often the watcher checks `stop`, and the pause after a failed accept.
const STOP_POLL: Duration = Duration::from_millis(50);

fn handle_client<H: Handler + ?Sized>(stream: Stream, handler: &H) -> Result<(), IpcError> {
    let mut stream = BufReader::new(stream);
    loop {
        let mut buf = String::new();
        let n = stream.read_line(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let req: Request = match decode_line(&buf) {
            Ok(req) => req,
            Err(err) => {
                write_response(
                    stream.get_mut(),
                    &Response {
                        id: 0,
                        result: None,
                        error: Some(RpcErrorBody::new("invalid_request", err.to_string())),
                    },
                )?;
                continue;
            }
        };

        if req.method == "subscribe" {
            write_response(
                stream.get_mut(),
                &Response {
                    id: req.id,
                    result: Some(serde_json::json!({})),
                    error: None,
                },
            )?;
            let rx = handler.subscribe();
            return stream_activity(&mut stream, rx);
        }

        let response = match handler.call(&req.method, req.params) {
            Ok(result) => Response {
                id: req.id,
                result: Some(result),
                error: None,
            },
            Err(error) => Response {
                id: req.id,
                result: None,
                error: Some(error),
            },
        };
        write_response(stream.get_mut(), &response)?;
    }
}

fn stream_activity(
    stream: &mut BufReader<Stream>,
    rx: mpsc::Receiver<ActivityItem>,
) -> Result<(), IpcError> {
    for item in rx {
        let line = encode_line(&item).map_err(IpcError::codec)?;
        if writeln!(stream.get_mut(), "{line}").is_err() {
            break;
        }
        if stream.get_mut().flush().is_err() {
            break;
        }
    }
    Ok(())
}

fn write_response(stream: &mut Stream, response: &Response) -> Result<(), IpcError> {
    let line = encode_line(response).map_err(IpcError::codec)?;
    writeln!(stream, "{line}")?;
    stream.flush()?;
    Ok(())
}

#[cfg(unix)]
fn bind_listener(endpoint: &Endpoint) -> Result<Listener, IpcError> {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};

    match endpoint {
        Endpoint::SocketFile(path) => {
            let name = path.clone().to_fs_name::<GenericFilePath>()?;
            Ok(ListenerOptions::new()
                .name(name)
                .reclaim_name(true)
                .create_sync()?)
        }
        Endpoint::NamedPipe(_) => Err(IpcError::codec(
            "namespaced pipes are not used on this platform",
        )),
    }
}

#[cfg(windows)]
fn bind_listener(endpoint: &Endpoint) -> Result<Listener, IpcError> {
    use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToNsName};

    match endpoint {
        Endpoint::NamedPipe(name) => {
            let name = name.as_str().to_ns_name::<GenericNamespaced>()?;
            Ok(ListenerOptions::new()
                .name(name)
                .reclaim_name(true)
                .create_sync()?)
        }
        Endpoint::SocketFile(_) => Err(IpcError::codec(
            "filesystem sockets are not used on this platform",
        )),
    }
}
