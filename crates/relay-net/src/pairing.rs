use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use quinn::{Connection, RecvStream, SendStream};
use relay_core::{DeviceId, PairingCode, collect_peer_addresses};
use relay_proto::pairing_message::Body;
use relay_proto::{
    PAIR_CONFIRM_A, PAIR_CONFIRM_B, PAIR_ID_INITIATOR, PAIR_ID_JOINER, PairConfirm, PairConfirmA,
    PairDeviceInfo, PairJoin, PairStart, PairingMessage, encode_frame,
};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use tokio::time::timeout;

use crate::NetEvent;
use crate::addr::advertised_addresses;
use crate::discovery::lookup_nameplate;
use crate::io::read_message_max;
use crate::session::{CLOSE_PROTOCOL, CLOSE_UNTRUSTED, Inner, close_code, peer_device_id};
use crate::tls::{SERVER_NAME, make_pairing_client_config};

pub(crate) const CLOSE_PAIRING: u32 = 6;
const JOIN_RESOLVE: Duration = Duration::from_secs(15);
const PAIR_IO: Duration = Duration::from_secs(20);
/// Addresses a device advertises while pairing; any beyond these are dropped.
const PAIR_ADDRESSES_MAX: usize = 16;
/// Pairing messages stay under 2 KiB: a name of up to 64 characters, SPAKE2
/// and MAC values of a few dozen bytes, and at most `PAIR_ADDRESSES_MAX`
/// addresses of under 50 bytes each. Any device may open a pairing stream
/// while a session is open, so the read buffer stays small.
const PAIR_MESSAGE_MAX: usize = 4 * 1024;

pub(crate) struct PairSession {
    pub code: PairingCode,
    pub expires_at: Instant,
    pub consumed: bool,
}

pub(crate) fn start_session(inner: &Arc<Inner>, code: String, expires_at: SystemTime) {
    let parsed = match PairingCode::parse(&code) {
        Ok(code) => code,
        Err(err) => {
            inner.emit(NetEvent::PairFailed {
                reason: err.to_string(),
            });
            return;
        }
    };
    let wait = expires_at
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO);
    let session = PairSession {
        code: parsed,
        expires_at: Instant::now() + wait,
        consumed: false,
    };
    {
        let mut slot = inner.pairing.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(session);
    }
    inner.refresh_pair_txt(Some(parsed.nameplate()));
    schedule_expiry(inner.clone(), parsed, wait);
}

fn schedule_expiry(inner: Arc<Inner>, code: PairingCode, wait: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        expire_if_still(&inner, code);
    });
}

fn expire_if_still(inner: &Inner, code: PairingCode) {
    let expired = {
        let mut slot = inner.pairing.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some(session)
                if session.code == code
                    && !session.consumed
                    && Instant::now() >= session.expires_at =>
            {
                *slot = None;
                true
            }
            _ => false,
        }
    };
    if expired {
        inner.refresh_pair_txt(None);
        inner.emit(NetEvent::PairFailed {
            reason: "pairing code expired".into(),
        });
    }
}

pub(crate) fn cancel_session(inner: &Inner) {
    let mut slot = inner.pairing.lock().unwrap_or_else(|e| e.into_inner());
    *slot = None;
    drop(slot);
    inner.refresh_pair_txt(None);
}

pub(crate) async fn accept_incoming(inner: Arc<Inner>, conn: Connection) {
    if !wait_session_open(&inner).await {
        conn.close(close_code(CLOSE_UNTRUSTED), b"no pairing session");
        return;
    }
    let peer_id = match peer_device_id(&conn) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(error = %err, "pairing: no peer identity");
            conn.close(close_code(CLOSE_PROTOCOL), b"no peer identity");
            return;
        }
    };
    if let Err(reason) = run_initiator(&inner, conn, peer_id).await {
        tracing::warn!(peer = %peer_id, %reason, "pairing initiator failed");
        inner.emit(NetEvent::PairFailed { reason });
    }
}

fn session_is_open(inner: &Inner) -> bool {
    let slot = inner.pairing.lock().unwrap_or_else(|e| e.into_inner());
    matches!(
        slot.as_ref(),
        Some(session) if !session.consumed && Instant::now() < session.expires_at
    )
}

async fn wait_session_open(inner: &Inner) -> bool {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if session_is_open(inner) {
            return true;
        }
        if Instant::now() >= deadline || inner.is_shutting_down() {
            return session_is_open(inner);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn run_initiator(inner: &Inner, conn: Connection, joiner: DeviceId) -> Result<(), String> {
    let (mut send, mut recv) = match timeout(PAIR_IO, conn.accept_bi()).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(err)) => {
            conn.close(close_code(CLOSE_PROTOCOL), b"pairing stream");
            return Err(format!("accept_bi: {err}"));
        }
        Err(_) => {
            conn.close(close_code(CLOSE_PROTOCOL), b"pairing timeout");
            return Err("timed out waiting for the joiner".into());
        }
    };

    let join = match read_pair(&mut recv)
        .await
        .map_err(|e| format!("initiator join: {e}"))?
    {
        Body::Join(join) => join,
        _ => {
            conn.close(close_code(CLOSE_PROTOCOL), b"expected join");
            return Err("first pairing message was not Join".into());
        }
    };

    let session_code = match take_matching_session(inner, &join.nameplate) {
        Take::Match(code) => code,
        Take::Ignore(reason) => {
            conn.close(close_code(CLOSE_UNTRUSTED), reason.as_bytes());
            return Ok(());
        }
        Take::Fail(reason) => {
            conn.close(close_code(CLOSE_UNTRUSTED), reason.as_bytes());
            return Err(reason);
        }
    };
    inner.refresh_pair_txt(None);

    let (state, msg_a) = Spake2::<Ed25519Group>::start_a(
        &Password::new(session_code.password_bytes()),
        &Identity::new(PAIR_ID_INITIATOR),
        &Identity::new(PAIR_ID_JOINER),
    );
    write_pair(
        &mut send,
        Body::Start(PairStart {
            spake2_a: msg_a.clone(),
        }),
    )
    .await?;

    let key = state
        .finish(&join.spake2_b)
        .map_err(|e| format!("initiator spake2: {e}"))?;
    let transcript = pair_transcript(
        session_code.nameplate(),
        inner.our_id,
        joiner,
        &join.spake2_b,
        &msg_a,
    );

    let confirm_b = match read_pair(&mut recv)
        .await
        .map_err(|e| format!("initiator confirm B: {e}"))?
    {
        Body::ConfirmB(c) => c,
        _ => {
            conn.close(close_code(CLOSE_PROTOCOL), b"expected confirm B");
            return Err("expected confirm B".into());
        }
    };
    let expected_b = confirm_mac(&key, PAIR_CONFIRM_B, &transcript);
    if !mac_matches(&confirm_b.mac, &expected_b) {
        conn.close(close_code(CLOSE_UNTRUSTED), b"confirm mismatch");
        return Err("pairing confirmation failed".into());
    }

    let confirm_a = confirm_mac(&key, PAIR_CONFIRM_A, &transcript);
    write_pair(
        &mut send,
        Body::ConfirmA(PairConfirmA {
            mac: confirm_a.as_bytes().to_vec(),
            info: Some(our_info(inner)),
        }),
    )
    .await?;

    let info = match read_pair(&mut recv)
        .await
        .map_err(|e| format!("initiator join info: {e}"))?
    {
        Body::JoinInfo(info) => info,
        _ => {
            conn.close(close_code(CLOSE_PROTOCOL), b"expected join info");
            return Err("expected joiner info".into());
        }
    };
    let addresses = collect_peer_addresses(&info.addresses, Some(conn.remote_address()));
    let name = sanitize_name(&info.name, joiner);
    conn.close(close_code(CLOSE_PAIRING), b"paired");
    inner.emit(NetEvent::Paired {
        peer: joiner,
        name,
        addresses,
        initiator: true,
    });
    Ok(())
}

enum Take {
    Match(PairingCode),
    Ignore(String),
    Fail(String),
}

fn take_matching_session(inner: &Inner, nameplate: &str) -> Take {
    let mut slot = inner.pairing.lock().unwrap_or_else(|e| e.into_inner());
    let Some(session) = slot.as_mut() else {
        return Take::Ignore("no pairing session".into());
    };
    if Instant::now() >= session.expires_at {
        *slot = None;
        return Take::Fail("pairing code expired".into());
    }
    if session.consumed {
        *slot = None;
        return Take::Fail("pairing session already used".into());
    }
    if session.code.nameplate() != nameplate {
        return Take::Ignore("pairing nameplate does not match".into());
    }
    session.consumed = true;
    let code = session.code;
    *slot = None;
    Take::Match(code)
}

pub(crate) async fn join(
    inner: Arc<Inner>,
    endpoint: quinn::Endpoint,
    code: String,
    addr: Option<String>,
) {
    let result = join_inner(&inner, &endpoint, code, addr).await;
    if let Err(reason) = result {
        inner.emit(NetEvent::PairFailed { reason });
    }
}

async fn join_inner(
    inner: &Inner,
    endpoint: &quinn::Endpoint,
    code: String,
    addr: Option<String>,
) -> Result<(), String> {
    let code = PairingCode::parse(&code).map_err(|e| e.to_string())?;
    let targets = resolve_join_addrs(inner, &code, addr).await?;
    let client = make_pairing_client_config(&inner.tls).map_err(|e| e.to_string())?;

    let mut last_err = "could not reach the other device".to_owned();
    for target in targets {
        match try_join_addr(inner, endpoint, &client, &code, target).await {
            Ok(()) => return Ok(()),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

async fn resolve_join_addrs(
    inner: &Inner,
    code: &PairingCode,
    addr: Option<String>,
) -> Result<Vec<String>, String> {
    if let Some(addr) = addr {
        return Ok(vec![addr]);
    }
    let deadline = Instant::now() + JOIN_RESOLVE;
    loop {
        let found = lookup_nameplate(&inner.pairing_ads, code.nameplate());
        if !found.is_empty() {
            return Ok(found);
        }
        if Instant::now() >= deadline {
            return Err(
                "no device on this network is showing that pairing code. Check the code, \
                 allow Relay local network access (macOS: System Settings > Privacy & \
                 Security > Local Network), or enter the other computer's address"
                    .into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        if inner.is_shutting_down() {
            return Err("network is shutting down".into());
        }
    }
}

async fn try_join_addr(
    inner: &Inner,
    endpoint: &quinn::Endpoint,
    client: &quinn::ClientConfig,
    code: &PairingCode,
    addr: String,
) -> Result<(), String> {
    let resolved = tokio::net::lookup_host(addr.as_str())
        .await
        .map_err(|e| format!("{addr}: {e}"))?
        .collect::<Vec<_>>();
    if resolved.is_empty() {
        return Err(format!("{addr}: no addresses"));
    }
    let mut last = format!("{addr}: connect failed");
    for sa in resolved {
        match endpoint.connect_with(client.clone(), sa, SERVER_NAME) {
            Ok(connecting) => match timeout(PAIR_IO, connecting).await {
                Ok(Ok(conn)) => match run_joiner(inner, conn, code).await {
                    Ok(()) => return Ok(()),
                    Err(err) => last = err,
                },
                Ok(Err(err)) => last = err.to_string(),
                Err(_) => last = format!("{sa}: handshake timed out"),
            },
            Err(err) => last = err.to_string(),
        }
    }
    Err(last)
}

async fn run_joiner(inner: &Inner, conn: Connection, code: &PairingCode) -> Result<(), String> {
    let initiator = peer_device_id(&conn).map_err(|e| e.to_string())?;
    tracing::debug!(peer = %initiator, alpn = ?negotiated_alpn(&conn), "pairing joiner: opening stream");
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi: {e}"))?;

    let (state, msg_b) = Spake2::<Ed25519Group>::start_b(
        &Password::new(code.password_bytes()),
        &Identity::new(PAIR_ID_INITIATOR),
        &Identity::new(PAIR_ID_JOINER),
    );
    write_pair(
        &mut send,
        Body::Join(PairJoin {
            nameplate: code.nameplate().to_owned(),
            spake2_b: msg_b.clone(),
        }),
    )
    .await?;

    let start = match read_pair(&mut recv)
        .await
        .map_err(|e| format!("joiner start: {e}"))?
    {
        Body::Start(start) => start,
        _ => {
            conn.close(close_code(CLOSE_PROTOCOL), b"expected start");
            return Err("expected SPAKE2 start".into());
        }
    };
    let key = state
        .finish(&start.spake2_a)
        .map_err(|e| format!("joiner spake2: {e}"))?;
    let transcript = pair_transcript(
        code.nameplate(),
        initiator,
        inner.our_id,
        &msg_b,
        &start.spake2_a,
    );
    write_pair(
        &mut send,
        Body::ConfirmB(PairConfirm {
            mac: confirm_mac(&key, PAIR_CONFIRM_B, &transcript)
                .as_bytes()
                .to_vec(),
        }),
    )
    .await?;

    let confirm_a = match read_pair(&mut recv)
        .await
        .map_err(|e| format!("joiner confirm A: {e}"))?
    {
        Body::ConfirmA(c) => c,
        _ => {
            conn.close(close_code(CLOSE_PROTOCOL), b"expected confirm A");
            return Err("expected confirm A".into());
        }
    };
    let expected_a = confirm_mac(&key, PAIR_CONFIRM_A, &transcript);
    if !mac_matches(&confirm_a.mac, &expected_a) {
        conn.close(close_code(CLOSE_UNTRUSTED), b"confirm mismatch");
        return Err("pairing confirmation failed".into());
    }
    let info = confirm_a.info.unwrap_or(PairDeviceInfo {
        name: String::new(),
        addresses: Vec::new(),
    });
    write_pair(&mut send, Body::JoinInfo(our_info(inner))).await?;
    let _ = send.finish();

    let addresses = collect_peer_addresses(&info.addresses, Some(conn.remote_address()));
    let name = sanitize_name(&info.name, initiator);
    let _ = timeout(PAIR_IO, conn.closed()).await;
    inner.emit(NetEvent::Paired {
        peer: initiator,
        name,
        addresses,
        initiator: false,
    });
    Ok(())
}

/// What this device tells the other about itself once the code has matched.
fn our_info(inner: &Inner) -> PairDeviceInfo {
    let mut addresses = advertised_addresses(inner.listen_port);
    addresses.truncate(PAIR_ADDRESSES_MAX);
    PairDeviceInfo {
        name: inner.device_name.clone(),
        addresses,
    }
}

fn pair_transcript(
    nameplate: &str,
    initiator: DeviceId,
    joiner: DeviceId,
    msg_b: &[u8],
    msg_a: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + 64 + msg_b.len() + msg_a.len());
    out.extend_from_slice(nameplate.as_bytes());
    out.extend_from_slice(initiator.as_bytes());
    out.extend_from_slice(joiner.as_bytes());
    out.extend_from_slice(msg_b);
    out.extend_from_slice(msg_a);
    out
}

fn confirm_mac(key: &[u8], label: &[u8], transcript: &[u8]) -> blake3::Hash {
    let mut keyed = [0u8; 32];
    if key.len() >= 32 {
        keyed.copy_from_slice(&key[..32]);
    } else {
        keyed = *blake3::hash(key).as_bytes();
    }
    let mut data = Vec::with_capacity(label.len() + transcript.len());
    data.extend_from_slice(label);
    data.extend_from_slice(transcript);
    blake3::keyed_hash(&keyed, &data)
}

fn mac_matches(received: &[u8], expected: &blake3::Hash) -> bool {
    <[u8; 32]>::try_from(received).is_ok_and(|bytes| blake3::Hash::from_bytes(bytes) == *expected)
}

/// A peer-supplied device name, or the id's short form when it would not
/// pass as a local name (empty, over 64 characters, control characters).
pub(crate) fn sanitize_name(name: &str, id: DeviceId) -> String {
    if relay_core::validate_name(name).is_ok() {
        name.to_owned()
    } else {
        id.short()
    }
}

async fn write_pair(send: &mut SendStream, body: Body) -> Result<(), String> {
    let bytes = encode_frame(&PairingMessage::new(body)).map_err(|e| e.to_string())?;
    send.write_all(&bytes)
        .await
        .map_err(|e| format!("write pairing: {e}"))
}

async fn read_pair(recv: &mut RecvStream) -> Result<Body, String> {
    let msg: PairingMessage = timeout(PAIR_IO, read_message_max(recv, PAIR_MESSAGE_MAX))
        .await
        .map_err(|_| "timed out reading a pairing message".to_owned())?
        .map_err(|e| e.to_string())?;
    msg.body.ok_or_else(|| "empty pairing message".to_owned())
}

pub(crate) fn negotiated_alpn(conn: &Connection) -> Option<Vec<u8>> {
    conn.handshake_data()?
        .downcast_ref::<quinn::crypto::rustls::HandshakeData>()?
        .protocol
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spake2_same_password_agrees() {
        let password = Password::new(b"1234567890");
        let (a, msg_a) = Spake2::<Ed25519Group>::start_a(
            &password,
            &Identity::new(PAIR_ID_INITIATOR),
            &Identity::new(PAIR_ID_JOINER),
        );
        let (b, msg_b) = Spake2::<Ed25519Group>::start_b(
            &password,
            &Identity::new(PAIR_ID_INITIATOR),
            &Identity::new(PAIR_ID_JOINER),
        );
        let ka = a.finish(&msg_b).unwrap();
        let kb = b.finish(&msg_a).unwrap();
        assert_eq!(ka, kb);
        assert_eq!(ka.len(), 32);
    }
}
