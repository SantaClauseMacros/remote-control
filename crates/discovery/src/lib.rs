//! LAN host discovery over mDNS.
//!
//! The host advertises `_remotecontrol._tcp.local.` while it is listening; a
//! client browses the same service type to populate "My Computers" without the
//! user typing an IP. Discovery carries only a friendly name, address and port
//! — never the pairing code or any key material.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

const SERVICE_TYPE: &str = "_remotecontrol._tcp.local.";

/// A host found on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundHost {
    /// Friendly computer name.
    pub name: String,
    /// Reachable address (first usable one advertised).
    pub addr: SocketAddr,
    /// Stable-ish device id the host published, if any.
    pub device_id: Option<String>,
}

/// Keeps an mDNS advertisement alive; unregisters on drop.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    /// Advertise this host. `name` is the friendly computer name; `device_id`
    /// (optional) lets a client match a previously-paired entry.
    pub fn start(name: &str, port: u16, device_id: Option<&str>) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("mDNS daemon")?;
        let instance = sanitize(name);
        let host_name = format!("{instance}.local.");

        let props: Vec<(String, String)> = vec![
            ("name".to_string(), name.to_string()),
            ("id".to_string(), device_id.unwrap_or_default().to_string()),
        ];

        // `enable_addr_auto` fills in this machine's addresses for us.
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host_name, "", port, &props[..])
            .context("building ServiceInfo")?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info).context("registering mDNS service")?;
        tracing::info!(%fullname, port, "advertising on the LAN via mDNS");
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Browse for hosts for `timeout`, returning what was seen (deduped by name).
pub fn discover(timeout: Duration) -> Result<Vec<FoundHost>> {
    let daemon = ServiceDaemon::new().context("mDNS daemon")?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .context("starting mDNS browse")?;

    let mut found: Vec<FoundHost> = Vec::new();
    let deadline = Instant::now() + timeout;
    while let Ok(event) = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        if let ServiceEvent::ServiceResolved(info) = event {
            let Some(ip) = info.get_addresses().iter().copied().next() else {
                continue;
            };
            let port = info.get_port();
            let props = info.get_properties();
            let name = props
                .get_property_val_str("name")
                .map(|s| s.to_string())
                .unwrap_or_else(|| trim_instance(info.get_fullname()));
            let device_id = props.get_property_val_str("id").map(|s| s.to_string());
            let host = FoundHost {
                name,
                addr: SocketAddr::new(ip, port),
                device_id,
            };
            if !found.iter().any(|h| h.name == host.name) {
                found.push(host);
            }
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    // Stop the browse but let the daemon wind itself down on drop — calling
    // shutdown() here races its own worker and logs a spurious error.
    let _ = daemon.stop_browse(SERVICE_TYPE);
    Ok(found)
}

/// Resolve one friendly name to an address by a short browse.
pub fn resolve(name: &str, timeout: Duration) -> Result<Option<SocketAddr>> {
    Ok(discover(timeout)?
        .into_iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.addr))
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() {
        "remote-control".into()
    } else {
        s
    }
}

fn trim_instance(fullname: &str) -> String {
    fullname
        .split_once('.')
        .map(|(a, _)| a.to_string())
        .unwrap_or_else(|| fullname.to_string())
}
