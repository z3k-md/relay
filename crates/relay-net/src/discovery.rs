use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use relay_core::DeviceId;

use crate::addr::advertised_ips;
use crate::session::Inner;

pub(crate) const SERVICE_TYPE: &str = "_relay._udp.local.";
/// Dial hints kept per trusted peer from its latest mDNS resolve. Few, so a
/// spoofed announcement can only add a couple of failed dial attempts.
const MAX_DISCOVERED: usize = 2;

pub(crate) struct Discovery {
    mdns: ServiceDaemon,
    fullname: String,
}

pub(crate) fn start(inner: &Arc<Inner>, port: u16) -> Option<Discovery> {
    let mdns = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!(error = %err, "mDNS unavailable; LAN discovery disabled");
            return None;
        }
    };
    let info = match build_info(&inner.our_id, &inner.device_name, port, None) {
        Ok(info) => info,
        Err(err) => {
            tracing::warn!(error = %err, "mDNS advertise failed");
            return None;
        }
    };
    let fullname = info.get_fullname().to_owned();
    if let Err(err) = mdns.register(info) {
        tracing::warn!(error = %err, "mDNS register failed");
        return None;
    }
    match mdns.browse(SERVICE_TYPE) {
        Ok(receiver) => {
            let inner = inner.clone();
            tokio::spawn(async move {
                browse_loop(inner, receiver).await;
            });
        }
        Err(err) => {
            tracing::warn!(error = %err, "mDNS browse failed");
        }
    }
    Some(Discovery { mdns, fullname })
}

impl Discovery {
    pub(crate) fn set_pair_nameplate(&self, nameplate: Option<&str>, inner: &Inner, port: u16) {
        match build_info(&inner.our_id, &inner.device_name, port, nameplate) {
            Ok(info) => {
                if let Err(err) = self.mdns.unregister(&self.fullname) {
                    tracing::debug!(error = %err, "mDNS unregister");
                }
                if let Err(err) = self.mdns.register(info) {
                    tracing::warn!(error = %err, "mDNS re-register failed");
                }
            }
            Err(err) => tracing::warn!(error = %err, "mDNS TXT update failed"),
        }
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.mdns.unregister(&self.fullname);
        let _ = self.mdns.shutdown();
    }
}

pub(crate) fn mdns_instance(id: &DeviceId) -> String {
    id.to_string()[..32].to_owned()
}

pub(crate) fn txt_records(
    id: &DeviceId,
    name: &str,
    pair: Option<&str>,
) -> HashMap<String, String> {
    let mut txt = HashMap::new();
    txt.insert("id".into(), id.to_string());
    txt.insert("name".into(), name.to_owned());
    txt.insert("v".into(), "1".into());
    if let Some(nameplate) = pair {
        txt.insert("pair".into(), nameplate.to_owned());
    }
    txt
}

fn build_info(
    id: &DeviceId,
    name: &str,
    port: u16,
    pair: Option<&str>,
) -> Result<ServiceInfo, String> {
    let instance = mdns_instance(id);
    let host = format!("{instance}.local.");
    let txt = txt_records(id, name, pair);
    let ips = advertised_ips();
    let info = if ips.is_empty() {
        ServiceInfo::new(SERVICE_TYPE, &instance, &host, (), port, txt)
            .map_err(|e| e.to_string())?
            .enable_addr_auto()
    } else {
        ServiceInfo::new(SERVICE_TYPE, &instance, &host, &*ips, port, txt)
            .map_err(|e| e.to_string())?
    };
    Ok(info)
}

async fn browse_loop(inner: Arc<Inner>, receiver: mdns_sd::Receiver<ServiceEvent>) {
    loop {
        let event = tokio::task::spawn_blocking({
            let receiver = receiver.clone();
            move || receiver.recv_timeout(Duration::from_millis(500))
        })
        .await;
        if inner.is_shutting_down() {
            return;
        }
        let Ok(Ok(event)) = event else {
            continue;
        };
        match event {
            ServiceEvent::ServiceResolved(info) => handle_resolved(&inner, &info),
            ServiceEvent::ServiceRemoved(_, fullname) => {
                forget_ad(&inner, &fullname);
            }
            _ => {}
        }
    }
}

fn handle_resolved(inner: &Inner, info: &ResolvedService) {
    let Some(id_str) = info.get_property_val_str("id") else {
        return;
    };
    let Ok(id) = id_str.parse::<DeviceId>() else {
        return;
    };
    if id == inner.our_id {
        return;
    }
    let addrs = resolved_addresses(info);
    if addrs.is_empty() {
        return;
    }
    if let Some(nameplate) = info.get_property_val_str("pair") {
        let mut ads = inner.pairing_ads.lock().unwrap_or_else(|e| e.into_inner());
        ads.insert(
            nameplate.to_owned(),
            PairingAd {
                fullname: info.get_fullname().to_owned(),
                addresses: addrs.clone(),
            },
        );
    } else {
        forget_ad(inner, info.get_fullname());
    }
    note_discovered(inner, id, addrs);
}

fn forget_ad(inner: &Inner, fullname: &str) {
    let mut ads = inner.pairing_ads.lock().unwrap_or_else(|e| e.into_inner());
    ads.retain(|_, ad| ad.fullname != fullname);
}

/// Remember where a trusted peer says it is, as a dial hint only. The
/// announcement is unauthenticated: anyone on the LAN can claim a peer's id,
/// so nothing here touches the stored address list. The dialer persists a
/// hint once a dial to it completed the pinned handshake.
fn note_discovered(inner: &Inner, id: DeviceId, discovered: Vec<String>) {
    if !inner.is_trusted(&id) {
        return;
    }
    let mut discovered = discovered;
    discovered.truncate(MAX_DISCOVERED);
    tracing::debug!(peer = %id, addresses = ?discovered, "mDNS dial hint");
    inner
        .discovered
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, discovered);
}

pub(crate) fn resolved_addresses(info: &ResolvedService) -> Vec<String> {
    let port = info.get_port();
    let mut out = Vec::new();
    for addr in info.get_addresses() {
        let ip = addr.to_ip_addr();
        if !relay_core::is_advertisable_ip(ip) {
            continue;
        }
        let formatted = relay_core::format_ip_port(ip, port);
        if !out.contains(&formatted) {
            out.push(formatted);
        }
    }
    out
}

pub(crate) struct PairingAd {
    pub fullname: String,
    pub addresses: Vec<String>,
}

pub(crate) fn lookup_nameplate(
    ads: &Mutex<HashMap<String, PairingAd>>,
    nameplate: &str,
) -> Vec<String> {
    ads.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(nameplate)
        .map(|ad| ad.addresses.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_core::DeviceId;

    #[test]
    fn instance_name_fits_dns_label() {
        let id = DeviceId::from_bytes([0xab; 32]);
        let name = mdns_instance(&id);
        assert!(name.len() <= 63);
        assert_eq!(name.len(), 32);
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    #[ignore = "multicast is flaky in CI"]
    fn mdns_end_to_end_nameplate() {
        let mdns = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(err) => {
                eprintln!("mDNS unavailable: {err}");
                return;
            }
        };
        let id = DeviceId::from_bytes([9; 32]);
        let info = build_info(&id, "tester", 47321, Some("42")).expect("service info");
        mdns.register(info).expect("register");
        let receiver = mdns.browse(SERVICE_TYPE).expect("browse");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut saw = false;
        while std::time::Instant::now() < deadline {
            if let Ok(ServiceEvent::ServiceResolved(info)) =
                receiver.recv_timeout(Duration::from_millis(200))
                && info.get_property_val_str("pair") == Some("42")
                && info.get_property_val_str("id") == Some(&id.to_string())
            {
                saw = true;
                break;
            }
        }
        let _ = mdns.shutdown();
        assert!(saw, "did not resolve the advertised pairing nameplate");
    }

    #[test]
    fn txt_omits_pair_unless_session_open() {
        let id = DeviceId::from_bytes([1; 32]);
        let idle = txt_records(&id, "mac", None);
        assert_eq!(idle.get("id").unwrap(), &id.to_string());
        assert_eq!(idle.get("name").unwrap(), "mac");
        assert_eq!(idle.get("v").unwrap(), "1");
        assert!(!idle.contains_key("pair"));

        let pairing = txt_records(&id, "mac", Some("42"));
        assert_eq!(pairing.get("pair").unwrap(), "42");
    }
}
