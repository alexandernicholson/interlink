# RFC 9110-9113 — HTTP Semantics & Versions

**Sources:**
- RFC 9110 (STD 97): HTTP Semantics — `/home/alex/rfcs/rfc9110.txt` (10785 lines)
- RFC 9112 (STD 99): HTTP/1.1 — `/home/alex/rfcs/rfc9112.txt`
- RFC 9113 (STD 98): HTTP/2 — `/home/alex/rfcs/rfc9113.txt` (4188 lines)

**Status:** Draft
**Priority:** HIGH — protocol detection and proxying

---

## 1. Summary

Three RFCs define modern HTTP: RFC 9110 provides the shared semantics (methods, status codes, headers, caching), RFC 9112 defines the HTTP/1.1 wire format (text-based, one request per connection), and RFC 9113 defines HTTP/2 (binary-framed, multiplexed streams over a single connection). For interlink, the key capability is **protocol detection**: reading the first few bytes of a TCP stream to determine whether it's HTTP/1.1, HTTP/2, or generic TCP, then routing accordingly with mTLS applied before forwarding.

---

## 2. Key Sections

### RFC 9113 (HTTP/2) — Critical for Proxy

| Section | Lines | Content |
|---------|-------|---------|
| 3.4 Connection Preface | 393-430 | Client sends `PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n` (24 bytes). Server sends SETTINGS frame. |
| 4.1 Frame Format | 450-510 | Header (9 bytes): Length(24), Type(8), Flags(8), StreamID(31), R(1). Our proxy MUST parse these. |
| 5.1 Stream States | 550-700 | idle → open → half-closed → closed. Critical for multiplexing. |
| 4.3 Field Compression | 430-450 | HPACK — we'll use `h2` crate but MUST understand for debugging. |
| 6.2 HEADERS / 6.1 DATA | frame defs | The core request/response frames. |
| 8.1 HTTP Request/Response | 2000+ | How HTTP semantics map to frames. The `:authority`, `:method`, `:path` pseudo-headers. |
| 9.2 TLS Configuration | 3500+ | ALPN negotiation: "h2" (0x68, 0x32). |

### Protocol Detection (first bytes heuristic)

```
Byte pattern         → Protocol
────────────────────────────────────────────────
PRI * HTTP/2.0       → HTTP/2 (connection preface, RFC 9113 §3.4)
GET / POST / PUT /... → HTTP/1.1 (method token, RFC 9112 §3)
<0x16 0x03>          → TLS handshake (not our concern — already inside TLS)
<binary>             → Raw TCP (pass through with mTLS wrapping)
```

The detection MUST happen after TLS termination (inside the mTLS tunnel). The proxy terminates mTLS, then inspects the plaintext first bytes.

---

## 3. Implementation Plan

```rust
/// Protocol detection from raw first bytes
pub enum DetectedProtocol {
    Http11,
    Http2,
    Grpc,            // HTTP/2 with specific content-type
    Tcp,             // fallback — raw TCP passthrough
}

pub struct ProtocolDetector;
impl ProtocolDetector {
    /// Detect protocol from first bytes of plaintext (post-TLS).
    /// Returns the protocol and how many bytes were consumed.
    pub fn detect(buf: &[u8]) -> DetectedProtocol {
        if buf.len() < 8 { return DetectedProtocol::Tcp }

        if buf.starts_with(b"PRI * HTTP/2.0") {
            return DetectedProtocol::Http2;
        }

        // HTTP/1.1 method sniff — RFC 9110 §9
        let methods = [b"GET ", b"POST", b"PUT ", b"DEL", b"HEAD",
                       b"PATC", b"OPTI", b"TRAC", b"CONN"];
        if methods.iter().any(|m| buf.starts_with(m)) {
            // Validate it's followed by SP — RFC 9112 §3
            let space_pos = buf.iter().position(|&b| b == b' ');
            if space_pos.unwrap_or(99) <= 8 {
                return DetectedProtocol::Http11;
            }
        }

        DetectedProtocol::Tcp
    }
}
```

### Proxy Routing

```
Client App (plaintext)           interlink Proxy                 Upstream
       │                              │                            │
       │── TCP :80 ──────────────────>│                            │
       │                              │ iptables redirects to      │
       │                              │ local proxy port           │
       │                              │                            │
       │                              ├── Detect protocol ────────▶│
       │                              │   from plaintext bytes      │
       │                              │                            │
       │   ┌─ HTTP/1.1 ──────────────┐│                            │
       │   │  Parse request line      ││ ── mTLS to upstream ────▶ │
       │   │  Forward to upstream      ││                            │
       │   └─────────────────────────┘│                            │
       │                              │                            │
       │   ┌─ HTTP/2 ────────────────┐│                            │
       │   │  Parse HTTP/2 preface    ││ ── mTLS to upstream ────▶ │
       │   │  Multiplex streams       ││                            │
       │   └─────────────────────────┘│                            │
```

---

## 4. Edge Cases

| Issue | RFC Reference | Mitigation |
|-------|---------------|------------|
| HTTP/1.1 pipeline | RFC 9112 §7 | Detect multiple requests in one read. Buffer and process sequentially. |
| HTTP/2 SETTINGS frame | RFC 9113 §6.5 | MUST process before sending HEADERS. Default max concurrent streams = 100. |
| HTTP/2 stream limits | RFC 9113 §5.1.2 | Enforce MAX_CONCURRENT_STREAMS. Reject with REFUSED_STREAM. |
| HTTP/1.1 to HTTP/2 conversion | RFC 9110 §18 | We DON'T do protocol conversion — preserve original. |
| gRPC detection | RFC 9113 + gRPC spec | Content-Type: application/grpc header in HTTP/2 headers. Pass through as HTTP/2. |
| Connection: keep-alive | RFC 9112 §9 | HTTP/1.1 persistent connections — MUST support. |
| Transfer-Encoding: chunked | RFC 9112 §6 | MUST handle for HTTP/1.1 passthrough. |

---

## 5. Compliance Checklist

### MUST
- [ ] Detect HTTP/2 from connection preface `PRI * HTTP/2.0` (RFC 9113 §3.4)
- [ ] Detect HTTP/1.1 from method token (RFC 9112 §3)
- [ ] Support ALPN negotiation for HTTP/2 over TLS: `h2` (RFC 9113 §3.2)
- [ ] Preserve original protocol version (no HTTP/1.1 ↔ HTTP/2 conversion)
- [ ] Parse HTTP/2 frame header (RFC 9113 §4.1)
- [ ] Handle HTTP/2 SETTINGS frames (RFC 9113 §6.5)
- [ ] Handle HTTP/2 GOAWAY frame gracefully (RFC 9113 §6.8)
- [ ] Validate HTTP/1.1 request line format (RFC 9112 §3)

### SHOULD
- [ ] Support HTTP/1.1 persistent connections (RFC 9112 §9)
- [ ] Support HTTP/2 flow control (RFC 9113 §5.2)
- [ ] Add `Forwarded` header for traceability (RFC 7239)

### MAY
- [ ] Support HTTP/2 server push (RFC 9113 §8.4) — unlikely needed in mesh
- [ ] Support HTTP/1.1 pipeline (RFC 9112 §7) — rare in practice
