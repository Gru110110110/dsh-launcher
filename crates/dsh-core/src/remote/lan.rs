//! LAN address discovery for the remote page's QR target.
//!
//! Uses the classic UDP-connect trick: connecting a UDP socket to a public
//! documentation address sends no packets, but forces the kernel to pick the
//! outgoing interface, revealing the primary LAN address. No dependency, no
//! traffic, works on macOS/Windows/Linux. When multiple interfaces exist
//! (VPN, Tailscale), the chosen address may not be reachable from the phone;
//! the UI copy tells the user to pick the address manually in that case.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// Best-effort primary LAN IPv4 of this machine, or None when the machine
/// has no non-loopback IPv4 route (offline, VPN-only, etc.).
pub fn primary_lan_ipv4() -> Option<Ipv4Addr> {
    // 192.0.2.1 is TEST-NET-1 (RFC 5737): reserved for documentation and
    // guaranteed unroutable, so connecting never emits a packet.
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_never_returns_loopback_or_panics() {
        // CI machines may be offline; the contract is only that a returned
        // address is a usable LAN address and the call never fails loudly.
        if let Some(address) = primary_lan_ipv4() {
            assert!(!address.is_loopback());
            assert!(!address.is_unspecified());
        }
    }
}
