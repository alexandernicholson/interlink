/// Default ports used by interlink proxy.
pub mod ports {
    pub const INBOUND_PROXY: u16 = 4143;
    pub const OUTBOUND_PROXY: u16 = 4140;
    pub const METRICS: u16 = 4190;
    pub const IDENTITY_GRPC: u16 = 4191;
    pub const ADMIN: u16 = 4192;
}

/// Default timeouts.
pub mod timeouts {
    use std::time::Duration;

    /// TLS 1.3 handshake timeout (RFC 8446 §4).
    pub const TLS_HANDSHAKE: Duration = Duration::from_secs(10);
    pub const TCP_IDLE: Duration = Duration::from_secs(300);
    pub const DNS_RESOLVE: Duration = Duration::from_secs(5);
    pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
}

/// Certificate defaults.
pub mod cert {
    use std::time::Duration;
    pub const LEAF_TTL: Duration = Duration::from_secs(86400);
    pub const ROOT_CA_TTL: Duration = Duration::from_secs(365 * 86400);
    pub const CLOCK_SKEW_TOLERANCE: Duration = Duration::from_secs(3600);
}

/// Buffer sizes tuned for edge performance.
pub mod buffers {
    pub const PROTOCOL_DETECT: usize = 32;
    pub const SOCKET_READ: usize = 16384;
    pub const MAX_FRAME_SIZE: u32 = 16_777_215;
}
