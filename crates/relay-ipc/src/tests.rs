use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::protocol::{
    ActivityItem, Hello, HostKind, HostState, PROTOCOL_VERSION, Request, Response, RpcErrorBody,
    Status, decode_line, encode_line,
};
use crate::{Client, Handler, Server};

struct TestHandler {
    hello: Hello,
    items: Vec<ActivityItem>,
    subscribers: std::sync::Mutex<Vec<mpsc::Sender<ActivityItem>>>,
}

impl TestHandler {
    fn new() -> Self {
        Self {
            hello: Hello {
                protocol: PROTOCOL_VERSION,
                relay_version: "0.1.0".into(),
                host: HostKind::Cli,
                pid: 7,
                started_at_ms: 1,
            },
            items: vec![ActivityItem {
                at_ms: 10,
                kind: "scan".into(),
                summary: "Personal/code scanned".into(),
                detail: None,
            }],
            subscribers: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl Handler for TestHandler {
    fn call(
        &self,
        method: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcErrorBody> {
        match method {
            "hello" => Ok(serde_json::to_value(&self.hello).unwrap()),
            "status" => Ok(serde_json::to_value(Status {
                state: HostState::Running,
                message: None,
                listen: Some("127.0.0.1:0".into()),
                peers: Vec::new(),
                mounts: Vec::new(),
            })
            .unwrap()),
            "activity" => Ok(serde_json::to_value(&self.items).unwrap()),
            other => Err(RpcErrorBody::new("unknown_method", other.to_string())),
        }
    }

    fn subscribe(&self) -> mpsc::Receiver<ActivityItem> {
        let (tx, rx) = mpsc::channel();
        for item in &self.items {
            let _ = tx.send(item.clone());
        }
        self.subscribers.lock().unwrap().push(tx);
        rx
    }
}

#[test]
fn codec_round_trips_request_and_response() {
    let req = Request {
        id: 3,
        method: "hello".into(),
        params: serde_json::json!({}),
    };
    let line = encode_line(&req).unwrap();
    let back: Request = decode_line(&line).unwrap();
    assert_eq!(req, back);

    let ok = Response {
        id: 3,
        result: Some(serde_json::json!({"protocol": 1})),
        error: None,
    };
    let err = Response {
        id: 4,
        result: None,
        error: Some(RpcErrorBody::new("paused", "host is paused")),
    };
    assert_eq!(ok, decode_line(&encode_line(&ok).unwrap()).unwrap());
    assert_eq!(err, decode_line(&encode_line(&err).unwrap()).unwrap());
}

#[test]
fn client_reports_no_host_in_empty_tempdir() {
    let dir = tempfile::TempDir::new().unwrap();
    let result = Client::connect(dir.path()).unwrap();
    assert!(result.is_none(), "expected no host, got a client");
}

#[test]
fn client_server_over_local_socket() {
    let dir = tempfile::TempDir::new().unwrap();
    let handler = Arc::new(TestHandler::new());
    let server = Server::bind(dir.path()).expect("bind ipc server");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let accept = thread::spawn(move || server.serve(handler, &stop_thread));

    let mut client = wait_client(dir.path()).expect("client should connect");
    let hello = client.hello().unwrap();
    assert_eq!(hello.protocol, PROTOCOL_VERSION);
    assert_eq!(hello.host, HostKind::Cli);
    assert_eq!(hello.pid, 7);

    let status = client.status().unwrap();
    assert_eq!(status.state, HostState::Running);
    assert_eq!(status.listen.as_deref(), Some("127.0.0.1:0"));

    let activity = client.activity(None).unwrap();
    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].kind, "scan");

    let mut sub = client.subscribe().unwrap();
    let first = sub.next_item().unwrap().expect("activity item");
    assert_eq!(first.summary, "Personal/code scanned");

    stop.store(true, Ordering::SeqCst);
    let _ = accept.join();
}

fn wait_client(home: &std::path::Path) -> Option<Client> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(Some(client)) = Client::connect(home) {
            return Some(client);
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}
