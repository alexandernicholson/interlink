use thiserror::Error;

#[derive(Error, Debug)]
pub enum InterlinkError {
    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Identity error: {0}")]
    Identity(String),

    #[error("Policy violation: {0}")]
    PolicyViolation(String),

    #[error("DNS resolution failed: {0}")]
    DnsResolution(String),

    #[error("Configuration error: {0}")]
    Config(String),
}
