// ---------------------------------------------------------------------------
// Native UPnP IGD port forwarding.
//
// librqbit's built-in forwarder only recognises `WANIPConnection:1` (IGD v1).
// Modern routers — UniFi UDM, many others — advertise only `WANIPConnection:2`
// (IGD v2), so librqbit discovers the gateway but never opens the port. This
// self-contained client speaks both versions: SSDP-discovers the IGD, reads
// its description, and renews an AddPortMapping lease over SOAP.
// ---------------------------------------------------------------------------

use anyhow::{anyhow, bail, Context, Result};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const SSDP_ADDR: &str = "239.255.255.250:1900";
const LEASE_SECS: u32 = 3600;
const PERMANENT_ONLY: &str = "the router only accepts permanent mappings";

struct Igd {
    control_url: String,
    service_type: String,
    local_ip: Ipv4Addr,
}

/// The IPv4 default-route gateway (macOS `route -n get default`).
/// Absolute path: a Finder-launched app has a minimal PATH.
fn default_gateway() -> Option<Ipv4Addr> {
    let out = std::process::Command::new("/sbin/route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?;
    for line in s.lines() {
        if let Some(rest) = line.trim().strip_prefix("gateway:") {
            if let Ok(ip) = rest.trim().parse::<Ipv4Addr>() {
                return Some(ip);
            }
        }
    }
    None
}

/// Local IPv4 to advertise as the internal client: the explicit interface's
/// address when `dev` is set (macOS `ipconfig getifaddr`), else the address
/// of the interface that reaches the LAN gateway. We target the gateway
/// (not a public IP) so VPN/tunnel interfaces — which never carry the LAN
/// subnet — can't hijack the choice and break SSDP multicast.
fn local_ipv4(dev: Option<&str>) -> Option<Ipv4Addr> {
    if let Some(dev) = dev {
        if let Ok(out) = std::process::Command::new("/usr/sbin/ipconfig")
            .args(["getifaddr", dev])
            .output()
        {
            if let Ok(s) = String::from_utf8(out.stdout) {
                if let Ok(ip) = s.trim().parse() {
                    return Some(ip);
                }
            }
        }
    }
    let target = default_gateway()
        .map(|gw| format!("{gw}:1900"))
        .unwrap_or_else(|| "8.8.8.8:80".to_string());
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(&target).ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v4) => Some(v4),
        _ => None,
    }
}

/// Send an SSDP M-SEARCH and return the first IGD LOCATION. We send to the
/// multicast group *and* unicast straight to the gateway — the unicast
/// sidesteps multicast egress-interface selection, which VPN/tunnel
/// interfaces routinely hijack on multi-interface macOS.
fn ssdp_discover(local_ip: Ipv4Addr, gateway: Option<Ipv4Addr>) -> Result<String> {
    let sock = UdpSocket::bind((local_ip, 0)).context("bind SSDP socket")?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    let msg = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_ADDR}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\n\
         ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n"
    );
    for _ in 0..3 {
        let _ = sock.send_to(msg.as_bytes(), SSDP_ADDR);
        if let Some(gw) = gateway {
            let _ = sock.send_to(msg.as_bytes(), (gw, 1900));
        }
    }
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let resp = String::from_utf8_lossy(&buf[..n]);
                for line in resp.lines() {
                    if line.get(..9).is_some_and(|h| h.eq_ignore_ascii_case("location:")) {
                        return Ok(line[9..].trim().to_string());
                    }
                }
            }
            Err(_) => break,
        }
    }
    bail!("no IGD responded to SSDP")
}

fn authority_root(url: &str) -> String {
    if let Some(i) = url.find("://") {
        let after = &url[i + 3..];
        let end = after.find('/').map(|p| i + 3 + p).unwrap_or(url.len());
        return url[..end].to_string();
    }
    url.to_string()
}

fn resolve_url(location: &str, base: Option<&str>, control: &str) -> String {
    if control.starts_with("http://") || control.starts_with("https://") {
        return control.to_string();
    }
    let root = base
        .filter(|b| b.starts_with("http"))
        .map(|b| b.trim_end_matches('/').to_string())
        .unwrap_or_else(|| authority_root(location));
    let root = root.trim_end_matches('/');
    if control.starts_with('/') {
        format!("{root}{control}")
    } else {
        format!("{root}/{control}")
    }
}

/// Fetch and parse the IGD description, picking the WAN connection service
/// (either version). Returns its absolute control URL + service type.
async fn fetch_igd(http: &reqwest::Client, location: &str, local_ip: Ipv4Addr) -> Result<Igd> {
    let xml = http
        .get(location)
        .timeout(Duration::from_secs(8))
        .send()
        .await?
        .text()
        .await?;
    let doc = roxmltree::Document::parse(&xml).context("parse IGD description")?;
    let base = doc
        .descendants()
        .find(|n| n.tag_name().name() == "URLBase")
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string());

    // Prefer WANIPConnection; fall back to WANPPPConnection. Any version.
    let mut found: Option<(String, String)> = None;
    for svc in doc.descendants().filter(|n| n.tag_name().name() == "service") {
        let st = svc
            .children()
            .find(|n| n.tag_name().name() == "serviceType")
            .and_then(|n| n.text())
            .unwrap_or("")
            .trim()
            .to_string();
        if !(st.contains("WANIPConnection") || st.contains("WANPPPConnection")) {
            continue;
        }
        let cu = svc
            .children()
            .find(|n| n.tag_name().name() == "controlURL")
            .and_then(|n| n.text())
            .unwrap_or("")
            .trim()
            .to_string();
        if cu.is_empty() {
            continue;
        }
        let prefer = st.contains("WANIPConnection");
        if found.is_none() || prefer {
            found = Some((st, cu));
            if prefer {
                break;
            }
        }
    }
    let (service_type, control) = found.ok_or_else(|| anyhow!("no WAN connection service"))?;
    Ok(Igd {
        control_url: resolve_url(location, base.as_deref(), &control),
        service_type,
        local_ip,
    })
}

async fn add_mapping(http: &reqwest::Client, igd: &Igd, port: u16, proto: &str) -> Result<()> {
    match add_mapping_for(http, igd, port, proto, LEASE_SECS).await {
        // Many older routers refuse timed leases (error 725) and only take
        // permanent ones; renewing it keeps it pointed at us.
        Err(e) if e.to_string() == PERMANENT_ONLY => {
            add_mapping_for(http, igd, port, proto, 0).await
        }
        other => other,
    }
}

async fn add_mapping_for(http: &reqwest::Client, igd: &Igd, port: u16, proto: &str, lease: u32) -> Result<()> {
    let action = format!("{}#AddPortMapping", igd.service_type);
    let body = format!(
        "<?xml version=\"1.0\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:AddPortMapping xmlns:u=\"{st}\">\
         <NewRemoteHost></NewRemoteHost>\
         <NewExternalPort>{port}</NewExternalPort>\
         <NewProtocol>{proto}</NewProtocol>\
         <NewInternalPort>{port}</NewInternalPort>\
         <NewInternalClient>{ip}</NewInternalClient>\
         <NewEnabled>1</NewEnabled>\
         <NewPortMappingDescription>KDownloader</NewPortMappingDescription>\
         <NewLeaseDuration>{lease}</NewLeaseDuration>\
         </u:AddPortMapping></s:Body></s:Envelope>",
        st = igd.service_type,
        port = port,
        proto = proto,
        ip = igd.local_ip,
        lease = lease,
    );
    let resp = http
        .post(&igd.control_url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPAction", format!("\"{action}\""))
        .timeout(Duration::from_secs(8))
        .body(body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let code = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        if txt.contains("OnlyPermanentLeasesSupported") || txt.contains("<errorCode>725</errorCode>") {
            bail!(PERMANENT_ONLY);
        }
        bail!("AddPortMapping {code}: {}", txt.chars().take(200).collect::<String>());
    }
    Ok(())
}

/// Discover the IGD and (re)create the port mapping, renewing the lease.
pub async fn run(http: reqwest::Client, port: u16, dev: Option<String>, map_udp: bool) {
    loop {
        let dev2 = dev.clone();
        let discovered = tokio::task::spawn_blocking(move || {
            let ip = local_ipv4(dev2.as_deref()).ok_or_else(|| anyhow!("no local IPv4"))?;
            let gw = default_gateway();
            info!("upnp: discovering (local_ip={ip}, gateway={gw:?})");
            let loc = ssdp_discover(ip, gw)?;
            Ok::<(Ipv4Addr, String), anyhow::Error>((ip, loc))
        })
        .await;

        let result = match discovered {
            Ok(Ok((ip, loc))) => match fetch_igd(&http, &loc, ip).await {
                Ok(igd) => {
                    let tcp = add_mapping(&http, &igd, port, "TCP").await;
                    if map_udp {
                        if let Err(e) = add_mapping(&http, &igd, port, "UDP").await {
                            warn!("upnp: UDP mapping failed: {e:#}");
                        }
                    }
                    match tcp {
                        Ok(()) => {
                            info!(
                                "upnp: forwarded port {port} via {} (client {})",
                                igd.service_type, igd.local_ip
                            );
                            None
                        }
                        Err(e) => Some(e),
                    }
                }
                Err(e) => Some(e),
            },
            Ok(Err(e)) => Some(e),
            Err(e) => Some(anyhow!("join: {e}")),
        };

        let wait = if let Some(e) = result {
            warn!("upnp: {e:#} — retrying in 120s");
            120
        } else {
            (LEASE_SECS / 2) as u64 // renew before the lease expires
        };
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}
