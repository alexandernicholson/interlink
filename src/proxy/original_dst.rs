use std::os::unix::io::AsRawFd;

use tokio::net::TcpStream;

/// Linux netfilter `SO_ORIGINAL_DST` socket option value.
///
/// Defined in `<linux/netfilter_ipv4.h>` as `IP_NF_SO_ORIGINAL_DST` (80).
/// libc does not currently expose this constant, so we define it locally.
#[cfg(target_os = "linux")]
const SO_ORIGINAL_DST: libc::c_int = 80;

/// Recover the original destination IP:port from iptables REDIRECT/DNAT.
///
/// When iptables redirects a TCP connection to the proxy port, the original
/// destination is preserved in the conntrack table. This function uses
/// `SO_ORIGINAL_DST` (Linux netfilter) to retrieve it.
///
/// On non-Linux platforms, or if the getsockopt fails, returns None.
#[cfg(target_os = "linux")]
pub fn get_original_dst(stream: &TcpStream) -> Option<String> {
    let fd = stream.as_raw_fd();
    let mut sockaddr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut socklen = std::mem::size_of::<libc::sockaddr_in>() as u32;

    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IP,
            SO_ORIGINAL_DST,
            &mut sockaddr as *mut _ as *mut libc::c_void,
            &mut socklen as *mut u32,
        )
    };

    if ret != 0 {
        return None;
    }

    let port = u16::from_be(sockaddr.sin_port);
    let ip_bytes = sockaddr.sin_addr.s_addr.to_be_bytes();
    let ip = std::net::Ipv4Addr::new(ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3]);

    Some(format!("{}:{}", ip, port))
}

#[cfg(not(target_os = "linux"))]
pub fn get_original_dst(_stream: &TcpStream) -> Option<String> {
    None
}
