use crate::common::error::InterlinkError;

/// The detected application-layer protocol from raw connection bytes.
///
/// Detection happens after TLS termination (inside the mTLS tunnel).
/// The proxy inspects the plaintext first bytes to determine how to route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedProtocol {
    Http11,
    Http2,
    Tcp,
}

/// Protocol detector inspects raw bytes to determine the protocol.
///
/// Detection rules (RFC 9113 §3.4, RFC 9112 §3):
/// - "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" → HTTP/2 (24 byte preface)
/// - GET/POST/PUT/DELETE/HEAD/PATCH/OPTIONS/TRACE/CONNECT → HTTP/1.1
/// - Everything else → Raw TCP passthrough
pub struct ProtocolDetector;

impl ProtocolDetector {
    /// Detect protocol from the first bytes of plaintext (post-TLS).
    ///
    /// RFC 9113 §3.4: HTTP/2 connection preface starts with "PRI * HTTP/2.0"
    /// RFC 9112 §3: HTTP/1.1 request starts with a method token
    ///
    /// # Arguments
    /// * `buf` - At least the first 8 bytes of the plaintext stream
    pub fn detect(buf: &[u8]) -> DetectedProtocol {
        if buf.len() < 8 {
            return DetectedProtocol::Tcp;
        }

        // HTTP/2 connection preface — RFC 9113 §3.4
        if buf.starts_with(b"PRI * HTTP/2") {
            return DetectedProtocol::Http2;
        }

        // HTTP/1.1 method sniff — RFC 9110 §9, RFC 9112 §3
        // Methods are 3-7 uppercase ASCII chars followed by SP.
        // Match on the first 3 bytes of the method, then find the space.
        if buf.len() >= 4 && buf[0].is_ascii_uppercase() {
            let space = buf.iter().position(|&b| b == b' ');
            if let Some(pos) = space {
                if (3..=7).contains(&pos) {
                    // All chars before space must be uppercase alpha
                    let method = &buf[..pos];
                    if method.iter().all(|&b| b.is_ascii_uppercase()) {
                        // Check it's a known HTTP method or just treat as HTTP/1.1
                        return DetectedProtocol::Http11;
                    }
                }
            }
        }

        DetectedProtocol::Tcp
    }
}

/// Validate that an HTTP/1.1 request line is well-formed per RFC 9112 §3.
pub fn validate_http11_request_line(line: &[u8]) -> Result<(), InterlinkError> {
    // RFC 9112 §3: request-line = method SP request-target SP HTTP-version CRLF
    let parts: Vec<&[u8]> = line.splitn(3, |&b| b == b' ').collect();
    if parts.len() != 3 {
        return Err(InterlinkError::Protocol(
            "invalid request-line: need 3 parts".into(),
        ));
    }

    let method = parts[0];
    let target = parts[1];
    let version = parts[2];

    // Method must be a token (RFC 9110 §9.1)
    if method.is_empty() || !method.iter().all(|&b| b.is_ascii_uppercase() || b == b'-') {
        return Err(InterlinkError::Protocol("invalid method token".into()));
    }

    // Request-target must not be empty (RFC 9112 §3.2)
    if target.is_empty() {
        return Err(InterlinkError::Protocol("empty request-target".into()));
    }

    // HTTP-version must be HTTP/1.0 or HTTP/1.1 (RFC 9112 §2.6)
    let version_trimmed = version.strip_suffix(b"\r").unwrap_or(version);
    if version_trimmed != b"HTTP/1.0" && version_trimmed != b"HTTP/1.1" {
        return Err(InterlinkError::Protocol("unsupported HTTP version".into()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_http2_preface() {
        let preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        assert_eq!(ProtocolDetector::detect(preface), DetectedProtocol::Http2);
    }

    #[test]
    fn test_detect_http11_get() {
        let req = b"GET /api/v1/users HTTP/1.1\r\nHost: example.com\r\n";
        assert_eq!(ProtocolDetector::detect(req), DetectedProtocol::Http11);
    }

    #[test]
    fn test_detect_http11_post() {
        let req = b"POST /submit HTTP/1.1\r\nHost: example.com\r\n";
        assert_eq!(ProtocolDetector::detect(req), DetectedProtocol::Http11);
    }

    #[test]
    fn test_detect_http11_patch() {
        let req = b"PATCH /resource HTTP/1.1\r\n";
        assert_eq!(ProtocolDetector::detect(req), DetectedProtocol::Http11);
    }

    #[test]
    fn test_detect_tcp_fallback() {
        let data = b"\x00\x01\x02\x03\x04\x05\x06\x07";
        assert_eq!(ProtocolDetector::detect(data), DetectedProtocol::Tcp);
    }

    #[test]
    fn test_detect_empty_buffer() {
        assert_eq!(ProtocolDetector::detect(b""), DetectedProtocol::Tcp);
        assert_eq!(ProtocolDetector::detect(b"SHORT"), DetectedProtocol::Tcp);
    }

    #[test]
    fn test_validate_http11_get() {
        let result = validate_http11_request_line(b"GET /index.html HTTP/1.1\r");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_http11_missing_version() {
        let result = validate_http11_request_line(b"GET /index.html");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_http11_bad_method() {
        let result = validate_http11_request_line(b"get /index.html HTTP/1.1\r");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_http11_h2_version() {
        let result = validate_http11_request_line(b"GET / HTTP/2.0\r");
        assert!(result.is_err());
    }
}
