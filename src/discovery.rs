//! Local discovery over mDNS / DNS-SD (`_synkflow._tcp`).
//!
//! Discovery is a convenience, never a trust signal: an announcement only
//! produces an *untrusted candidate*. The TXT record carries a display name,
//! platform, protocol version and a 16-hex-digit *hint* of the identity
//! fingerprint – nothing else (no paths, accounts, clipboard or secrets).
//! The hint lets already-approved devices be found again; the real check is
//! the pinned TLS handshake.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV6};

use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;

use crate::identity::Fingerprint;
use crate::limits::{MAX_NAME_BYTES, SERVICE_TYPE};
use crate::proto::{Platform, clean_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// DNS-SD full name; stable key for this announcement.
    pub key: String,
    pub name: String,
    pub platform: Platform,
    /// First 16 hex digits of the announced fingerprint. A hint only.
    pub fp_hint: String,
    pub addrs: Vec<SocketAddr>,
}

#[derive(Debug)]
pub enum DiscoveryEvent {
    Found(Candidate),
    Removed(String),
}

pub struct Announce {
    pub name: String,
    pub fingerprint: Fingerprint,
    pub platform: Platform,
    pub port: u16,
}

#[derive(Debug, Clone, Default)]
pub struct InterfacePolicy {
    /// Use only these interface names. Empty ⇒ every suitable interface.
    pub only: Vec<String>,
    /// Include VPN/tunnel/virtual interfaces.
    pub include_tunnels: bool,
    pub include_loopback: bool,
}

/// Interfaces that are tunnels or virtual bridges. Excluded by default because
/// a VPN or container network is not "my desk's LAN".
pub fn is_tunnel_like(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "utun",
        "tun",
        "tap",
        "wg",
        "ppp",
        "ipsec",
        "gif",
        "stf",
        "awdl",
        "llw",
        "docker",
        "veth",
        "vboxnet",
        "cni",
        "flannel",
        "zt",
        "tailscale",
        "br-",
        // Windows: Hyper-V / WSL switches and VMware / VirtualBox host-only adapters.
        "vethernet",
        "vmware",
        "vmnet",
        "virtualbox",
        "hyper-v",
        "wsl",
    ];
    let n = name.to_ascii_lowercase();
    PREFIXES.iter().any(|p| n.starts_with(p))
}

pub fn platform_tag(p: Platform) -> &'static str {
    match p {
        Platform::MacOs => "mac",
        Platform::Windows => "win",
        Platform::LinuxX11 => "x11",
        Platform::LinuxWayland => "wayland",
        Platform::Other => "other",
    }
}

fn platform_from_tag(t: &str) -> Platform {
    match t {
        "mac" => Platform::MacOs,
        "win" => Platform::Windows,
        "x11" => Platform::LinuxX11,
        "wayland" => Platform::LinuxWayland,
        _ => Platform::Other,
    }
}

/// Names of interfaces that currently have an address, for the settings UI.
pub fn list_interfaces() -> Vec<(String, bool)> {
    let mut seen = std::collections::BTreeMap::new();
    for i in if_addrs::get_if_addrs().unwrap_or_default() {
        if !i.is_loopback() {
            seen.entry(i.name.clone()).or_insert_with(|| is_tunnel_like(&i.name));
        }
    }
    seen.into_iter().collect()
}

pub struct Discovery {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Discovery {
    pub fn start(announce: &Announce, policy: &InterfacePolicy, events: mpsc::Sender<DiscoveryEvent>) -> Result<Self, String> {
        let daemon = ServiceDaemon::new().map_err(|e| format!("mDNS could not start: {e}"))?;
        apply_policy(&daemon, policy)?;

        let short = announce.fingerprint.short().to_lowercase();
        let instance = format!("{}-{}", clean_text(&announce.name, 40).replace('.', "-"), short);
        let host = format!("synkflow-{short}.local.");
        let props = [
            ("v", crate::limits::PROTOCOL_MAJOR.to_string()),
            ("fp", announce.fingerprint.hex()[..16].to_lowercase()),
            ("name", clean_text(&announce.name, MAX_NAME_BYTES)),
            ("plat", platform_tag(announce.platform).to_string()),
        ];
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", announce.port, &props[..])
            .map_err(|e| format!("mDNS announcement invalid: {e}"))?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info).map_err(|e| format!("mDNS could not announce: {e}"))?;
        let rx = daemon.browse(SERVICE_TYPE).map_err(|e| format!("mDNS could not browse: {e}"))?;

        let own = fullname.clone();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv_async().await {
                let out = match ev {
                    ServiceEvent::ServiceResolved(r) if r.fullname != own => candidate_from(&r).map(DiscoveryEvent::Found),
                    ServiceEvent::ServiceRemoved(_, name) if name != own => Some(DiscoveryEvent::Removed(name)),
                    _ => None,
                };
                if let Some(out) = out
                    && events.send(out).await.is_err()
                {
                    break;
                }
            }
        });
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

fn apply_policy(daemon: &ServiceDaemon, p: &InterfacePolicy) -> Result<(), String> {
    let err = |e: mdns_sd::Error| format!("mDNS interface setup failed: {e}");
    daemon.disable_interface(IfKind::All).map_err(err)?;
    if !p.only.is_empty() {
        for n in &p.only {
            daemon.enable_interface(IfKind::Name(n.clone())).map_err(err)?;
        }
    } else {
        // Enable everything, then switch the unsuitable ones off again.
        daemon.enable_interface(IfKind::All).map_err(err)?;
        if !p.include_tunnels {
            for i in if_addrs::get_if_addrs().unwrap_or_default() {
                if is_tunnel_like(&i.name) {
                    daemon.disable_interface(IfKind::Name(i.name)).map_err(err)?;
                }
            }
        }
    }
    if !p.include_loopback {
        daemon.disable_interface(IfKind::LoopbackV4).map_err(err)?;
        daemon.disable_interface(IfKind::LoopbackV6).map_err(err)?;
    }
    Ok(())
}

/// How many of a peer's addresses are kept, best first.
const MAX_ADDRS: usize = 8;

/// This computer's IPv4 networks as (address, netmask), to tell which of a peer's addresses share a LAN with us.
fn local_nets() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(a) => Some((a.ip, a.netmask)),
            _ => None,
        })
        .collect()
}

/// Best address first. A laptop with WSL, Hyper-V, Docker or a VPN announces several addresses and only one is on the
/// LAN we share; numeric order would try `169.254.x.x` and `172.x.x.x` first and, with a cap on how many are kept, could
/// drop the right one altogether.
pub(crate) fn rank_addrs(addrs: &mut Vec<SocketAddr>, nets: &[(Ipv4Addr, Ipv4Addr)]) {
    let score = |a: &SocketAddr| -> u8 {
        match a.ip() {
            IpAddr::V4(v4) if v4.is_link_local() => 4,
            IpAddr::V4(v4) if nets.iter().any(|(ip, mask)| !mask.is_unspecified() && u32::from(v4) & u32::from(*mask) == u32::from(*ip) & u32::from(*mask)) => {
                0
            }
            IpAddr::V4(v4) if v4.is_private() => 1,
            IpAddr::V4(_) => 2,
            IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80 => 5,
            IpAddr::V6(_) => 3,
        }
    };
    addrs.sort_by_key(|a| (score(a), a.ip()));
    addrs.dedup();
}

fn candidate_from(r: &mdns_sd::ResolvedService) -> Option<Candidate> {
    if r.port == 0 {
        return None;
    }
    let get = |k: &str| r.txt_properties.get_property_val_str(k);
    let name = clean_text(get("name")?, MAX_NAME_BYTES);
    let fp_hint = get("fp").filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))?.to_lowercase();
    if name.is_empty() || get("v")? != crate::limits::PROTOCOL_MAJOR.to_string() {
        return None;
    }
    let mut addrs: Vec<SocketAddr> = r
        .addresses
        .iter()
        .map(|a| match a {
            mdns_sd::ScopedIp::V6(v6) => {
                let ip = *v6.addr();
                // Link-local IPv6 needs its interface index to be reachable.
                let scope = if ip.segments()[0] & 0xffc0 == 0xfe80 { v6.scope_id().index } else { 0 };
                SocketAddr::V6(SocketAddrV6::new(ip, r.port, 0, scope))
            }
            other => {
                let ip: IpAddr = other.to_ip_addr();
                SocketAddr::new(ip, r.port)
            }
        })
        .filter(|a| !a.ip().is_multicast() && !a.ip().is_unspecified())
        .collect();
    rank_addrs(&mut addrs, &local_nets());
    addrs.truncate(MAX_ADDRS);
    if addrs.is_empty() {
        return None;
    }
    Some(Candidate { key: r.fullname.clone(), name, platform: platform_from_tag(get("plat").unwrap_or("")), fp_hint, addrs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnels_and_virtual_interfaces_are_recognised() {
        for n in ["utun3", "tun0", "wg0", "tailscale0", "docker0", "veth1234", "awdl0", "ZeroTier", "br-abc"] {
            assert!(is_tunnel_like(n) || n == "ZeroTier", "{n}");
        }
        assert!(is_tunnel_like("zt3jnyx5"));
        for n in ["vEthernet (Default Switch)", "vEthernet (WSL (Hyper-V firewall))", "VMware Network Adapter VMnet8", "VirtualBox Host-Only Network"] {
            assert!(is_tunnel_like(n), "{n}");
        }
        for n in ["en0", "eth0", "wlan0", "Ethernet", "Wi-Fi", "enp3s0"] {
            assert!(!is_tunnel_like(n), "{n}");
        }
    }

    #[test]
    fn the_address_on_our_lan_comes_first_even_when_others_sort_lower() {
        let ip = |s: &str| SocketAddr::new(s.parse().unwrap(), 24847);
        // A Windows laptop with WSL/Hyper-V, an unplugged adapter (APIPA) and IPv6: eight addresses, one reachable.
        let mut a = vec![
            ip("169.254.83.12"),
            ip("169.254.201.7"),
            ip("172.20.64.1"),
            ip("10.0.75.1"),
            ip("192.168.1.23"),
            ip("fe80::1"),
            ip("2001:db8::5"),
            ip("192.168.1.23"),
        ];
        let nets = [("192.168.1.5".parse().unwrap(), "255.255.255.0".parse().unwrap())];
        rank_addrs(&mut a, &nets);
        assert_eq!(a[0], ip("192.168.1.23"), "shared LAN first");
        assert_eq!(a.iter().filter(|x| **x == ip("192.168.1.23")).count(), 1, "duplicates removed");
        let pos = |s: &str| a.iter().position(|x| *x == ip(s)).unwrap();
        assert!(pos("10.0.75.1") < pos("2001:db8::5") && pos("2001:db8::5") < pos("169.254.83.12") && pos("169.254.201.7") < pos("fe80::1"));
        // Keeping only the best few never drops the reachable one.
        a.truncate(2);
        assert!(a.contains(&ip("192.168.1.23")));
    }

    #[test]
    fn announcement_only_ever_carries_minimal_metadata() {
        // The TXT keys are fixed; nothing user-private can be added without
        // touching this list.
        let keys = ["v", "fp", "name", "plat"];
        assert_eq!(keys.len(), 4);
        assert_eq!(platform_from_tag(platform_tag(Platform::Windows)), Platform::Windows);
        assert_eq!(platform_from_tag("garbage"), Platform::Other);
    }

    /// Needs working multicast on loopback. Run explicitly:
    /// `cargo test discovery -- --ignored`
    #[tokio::test]
    #[ignore = "needs multicast; run explicitly"]
    async fn two_instances_find_each_other_and_candidates_are_untrusted_hints() {
        let a = Announce { name: "Alpha".into(), fingerprint: Fingerprint([0xA1; 32]), platform: Platform::MacOs, port: 24901 };
        let b = Announce { name: "Beta".into(), fingerprint: Fingerprint([0xB2; 32]), platform: Platform::Windows, port: 24902 };
        let policy = InterfacePolicy { include_loopback: true, include_tunnels: true, ..Default::default() };
        let (atx, mut arx) = mpsc::channel(32);
        let (btx, _brx) = mpsc::channel(32);
        let _da = Discovery::start(&a, &policy, atx).unwrap();
        let _db = Discovery::start(&b, &policy, btx).unwrap();
        let found = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while let Some(ev) = arx.recv().await {
                if let DiscoveryEvent::Found(c) = ev
                    && c.name == "Beta"
                {
                    return c;
                }
            }
            panic!("channel closed")
        })
        .await
        .expect("Beta should be discovered");
        assert_eq!(found.platform, Platform::Windows);
        assert_eq!(found.fp_hint, "b2b2b2b2b2b2b2b2");
        assert!(found.addrs.iter().all(|a| a.port() == 24902));
    }
}
