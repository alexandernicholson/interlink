use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;

use tokio::net::TcpStream;

/// Linux netfilter `SO_ORIGINAL_DST` socket option value.
/// Defined in `<linux/netfilter_ipv4.h>` as `IP_NF_SO_ORIGINAL_DST` (80).
#[cfg(target_os = "linux")]
const SO_ORIGINAL_DST: libc::c_int = 80;

/// Recover the original destination from iptables REDIRECT/DNAT.
///
/// Supports both IPv4 (`sockaddr_in` via `SO_ORIGINAL_DST`) and
/// IPv6 (`sockaddr_in6` via `IP6T_SO_ORIGINAL_DST` = 80).
///
/// Returns a `SocketAddr` to avoid String formatting and re-parsing.
#[cfg(target_os = "linux")]
pub fn get_original_dst(stream: &TcpStream) -> Option<SocketAddr> {
    let fd = stream.as_raw_fd();

    // Try IPv4 first.
    let mut v4: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len4 = std::mem::size_of::<libc::sockaddr_in>() as u32;
    let ret4 = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IP,
            SO_ORIGINAL_DST,
            &mut v4 as *mut _ as *mut libc::c_void,
            &mut len4 as *mut u32,
        )
    };
    if ret4 == 0 {
        let port = u16::from_be(v4.sin_port);
        let ip = std::net::Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr));
        return Some(SocketAddr::new(std::net::IpAddr::V4(ip), port));
    }

    // Try IPv6.
    let mut v6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    let mut len6 = std::mem::size_of::<libc::sockaddr_in6>() as u32;
    let ret6 = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IPV6,
            SO_ORIGINAL_DST,
            &mut v6 as *mut _ as *mut libc::c_void,
            &mut len6 as *mut u32,
        )
    };
    if ret6 == 0 {
        let port = u16::from_be(v6.sin6_port);
        let ip = std::net::Ipv6Addr::from(v6.sin6_addr.s6_addr);
        return Some(SocketAddr::new(std::net::IpAddr::V6(ip), port));
    }

    None
}

#[cfg(not(target_os = "linux"))]
pub fn get_original_dst(_stream: &TcpStream) -> Option<SocketAddr> {
    None
}
