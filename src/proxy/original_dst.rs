#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::net::SocketAddr;
#[cfg(target_os = "linux")]
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
    let mut len4 = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let ret4 = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IP,
            SO_ORIGINAL_DST,
            &mut v4 as *mut _ as *mut libc::c_void,
            &mut len4,
        )
    };
    if ret4 == 0 {
        return decode_v4(&v4, len4);
    }

    // Try IPv6.
    let mut v6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    let mut len6 = std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
    let ret6 = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IPV6,
            SO_ORIGINAL_DST,
            &mut v6 as *mut _ as *mut libc::c_void,
            &mut len6,
        )
    };
    if ret6 == 0 {
        return decode_v6(&v6, len6);
    }

    None
}

#[cfg(any(target_os = "linux", test))]
fn decode_v4(addr: &libc::sockaddr_in, len: libc::socklen_t) -> Option<SocketAddr> {
    if len as usize != std::mem::size_of::<libc::sockaddr_in>()
        || i32::from(addr.sin_family) != libc::AF_INET
    {
        return None;
    }
    Some(SocketAddr::new(
        std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)).into(),
        u16::from_be(addr.sin_port),
    ))
}

#[cfg(any(target_os = "linux", test))]
fn decode_v6(addr: &libc::sockaddr_in6, len: libc::socklen_t) -> Option<SocketAddr> {
    if len as usize != std::mem::size_of::<libc::sockaddr_in6>()
        || i32::from(addr.sin6_family) != libc::AF_INET6
    {
        return None;
    }
    Some(SocketAddr::V6(std::net::SocketAddrV6::new(
        std::net::Ipv6Addr::from(addr.sin6_addr.s6_addr),
        u16::from_be(addr.sin6_port),
        u32::from_be(addr.sin6_flowinfo),
        addr.sin6_scope_id,
    )))
}

#[cfg(not(target_os = "linux"))]
pub fn get_original_dst(_stream: &TcpStream) -> Option<SocketAddr> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_dst_validates_v4_length_and_family() {
        let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        addr.sin_family = libc::AF_INET as _;
        addr.sin_port = 8080u16.to_be();
        addr.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
        let len = std::mem::size_of_val(&addr) as libc::socklen_t;
        assert_eq!(
            decode_v4(&addr, len),
            Some("127.0.0.1:8080".parse().unwrap())
        );
        assert_eq!(decode_v4(&addr, len - 1), None);
        assert_eq!(decode_v4(&addr, len + 1), None);
        addr.sin_family = libc::AF_INET6 as _;
        assert_eq!(decode_v4(&addr, len), None);
    }

    #[test]
    fn original_dst_validates_v6_length_and_family() {
        let mut addr: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
        addr.sin6_family = libc::AF_INET6 as _;
        addr.sin6_port = 8080u16.to_be();
        addr.sin6_addr.s6_addr = std::net::Ipv6Addr::LOCALHOST.octets();
        addr.sin6_scope_id = 3;
        let len = std::mem::size_of_val(&addr) as libc::socklen_t;
        assert_eq!(decode_v6(&addr, len), Some("[::1%3]:8080".parse().unwrap()));
        assert_eq!(decode_v6(&addr, len - 1), None);
        assert_eq!(decode_v6(&addr, len + 1), None);
        addr.sin6_family = libc::AF_INET as _;
        assert_eq!(decode_v6(&addr, len), None);
    }
}
