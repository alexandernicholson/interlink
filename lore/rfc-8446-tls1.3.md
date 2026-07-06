# RFC 8446 — TLS 1.3 Implementation Plan

**Source:** `/home/alex/rfcs/rfc8446.txt` (8963 lines)
**Status:** Draft
**Priority:** HIGH — core protocol for all mTLS communication

---

## 1. Summary

TLS 1.3 provides a secure channel between two peers with server authentication (mandatory) and client authentication (optional, but **mandatory for interlink's mTLS**). The handshake uses (EC)DHE key exchange for forward secrecy, AEAD ciphers (AES-256-GCM or ChaCha20-Poly1305) for encryption, and HKDF for key derivation. All handshake messages after ServerHello are encrypted. PSK resumption enables 0-RTT data. TLS 1.3 obsoletes TLS 1.2 (RFC 5246), 1.1, 1.0, SSLv3, and the static RSA key exchange.

**Key innovation for interlink:** The `CertificateRequest` message (Section 4.3.2, line 657) makes this mTLS — the server asks for a client cert, enabling bidirectional authentication.

---

## 2. Key Sections

| Section | Lines | Content |
|---------|-------|---------|
| 1 Introduction | 287-349 | Goals: auth, confidentiality, integrity. Note Section 1 bullet: client auth is "optional" — in interlink it's mandatory. |
| 1.2 Major Differences | 405-472 | Removed static RSA, DHE, compression. Must-read for what NOT to implement. |
| 2 Protocol Overview | 511-715 | Full handshake diagram (Figure 1, lines 569-601). This is THE reference for our proxy. |
| 2.2 Resumption/PSK | 791-916 | PSK-based resumption (Figure 3, lines 850-881). Useful for proxy-to-proxy connection pooling. |
| 4 Handshake Protocol | 1317-1400 | Handshake struct, message ordering. Messages MUST be in order (line 1383). |
| 4.1.1 Crypto Negotiation | 1407-1483 | Client offers cipher suites, groups, signature algorithms, PSKs. |
| 4.1.2 ClientHello | 1485-1600 | Structure: legacy_version=0x0303, random[32], cipher_suites, extensions. |
| 4.1.3 ServerHello | 1601-1750 | Server chooses parameters, provides key_share. |
| 4.2.3 Signature Algorithms | 2300-2400 | rsa_pss_rsae_sha256 (0x0804) and ed25519 (0x0807) are our primary targets. |
| 4.2.7 Supported Groups | 2600-2662 | x25519(0x001D) is the MUST-implement group. secp256r1 is acceptable fallback. |
| 4.2.8 Key Share | 2663-2776 | DH public values. x25519 keys are 32 bytes. |
| 4.4.2 Certificate | ~3200-3350 | X.509v3 certificate with SPIFFE ID in SAN URI. |
| 4.4.3 CertificateVerify | ~3400-3500 | Signature over the handshake transcript. |
| 4.4.4 Finished | ~3500-3600 | HMAC over handshake, provides key confirmation. |
| 9.1 Mandatory Ciphers | 100 lines before | TLS_AES_128_GCM_SHA256 (0x1301) is the MUST. TLS_CHACHA20_POLY1305_SHA256 is the SHOULD. |
| 10 Security Considerations | ~7700-8000 | Downgrade protection, side channels, PSK binder attacks. |

---

## 3. Implementation Plan

### 3.1 Abstraction

```rust
/// A TLS 1.3 session. Wraps rustls with interlink-specific identity binding.
pub struct TlsSession {
    state: SessionState,       // Handshake state machine (RFC §4.1)
    cipher: CipherSuite,       // Negotiated AEAD + HKDF hash (RFC §9.1)
    peer_identity: SpiffeId,   // SAN URI from peer cert (RFC 5280 §4.2.1.6)
    local_identity: SpiffeId,  // Our identity
    transcript: TranscriptHash, // Handshake transcript for verification
}

// Two roles, same type:
pub struct TlsClient {
    config: Arc<ClientConfig>,
    provider: Arc<IdentityProvider>,
}

pub struct TlsServer {
    config: Arc<ServerConfig>,
    provider: Arc<IdentityProvider>,
}
```

### 3.2 Handshake Protocol Mapping

| Handshake Message | Line Ref | Interlink Handler | State Transition |
|---|---|---|---|
| ClientHello | 1546-1553 | `build_client_hello()` | Client → ServerHello |
| ServerHello | 1601+ | `parse_server_hello()` | Server → EncryptedExtensions |
| EncryptedExtensions | ~2100+ | `parse_encrypted_ext()` | Server → Certificate |
| CertificateRequest | ~4000+ | `build_certificate()` | Trigger client auth |
| Certificate | ~3200+ | `verify_chain()` + `extract_spiffe_id()` | Auth phase |
| CertificateVerify | ~3400+ | `verify_signature()` | Verify transcript sig |
| Finished | ~3500+ | `verify_finished()` | Key confirmation |

### 3.3 Cipher Suite Priority

```rust
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CipherSuite {
    // RFC 8446 §9.1 — mandatory
    TlsAes128GcmSha256 = 0x1301,
    // RFC 8446 §9.1 — SHOULD
    TlsAes256GcmSha384 = 0x1302,
    // RFC 8446 §9.1 — SHOULD (preferred for mobile/ARM)
    TlsChaCha20Poly1305Sha256 = 0x1303,
}
```

Choice: **x25519 + TLS_CHACHA20_POLY1305_SHA256** as default (fast on ARM, no AES-NI dependency), fallback to **secp256r1 + TLS_AES_128_GCM_SHA256** for compatibility.

### 3.4 Signature Algorithms

| Algorithm | Code | Status |
|-----------|------|--------|
| ed25519 (RFC 8032) | 0x0807 | Primary — small keys, fast, constant-time |
| rsa_pss_rsae_sha256 | 0x0804 | Fallback — required by RFC 8446 |
| ecdsa_secp256r1_sha256 | 0x0403 | Secondary fallback |

### 3.5 X.509 Certificate Profile

Every interlink proxy cert:

```
Certificate:
    Version: 3 (0x2)
    Serial Number: <random 8 bytes>
    Signature Algorithm: ED25519
    Issuer: CN = interlink CA, O = interlink
    Validity:
        Not Before: <now>
        Not After : <now + 24h>
    Subject: (empty — identity is in SAN only)
    Subject Public Key Info: <x25519 or ed25519>
    X509v3 extensions:
        X509v3 Subject Alternative Name:
            URI: spiffe://<trust-domain>/ns/<ns>/sa/<sa>
        X509v3 Key Usage: Digital Signature, Key Encipherment
        X509v3 Extended Key Usage: TLS Web Server Authentication, TLS Web Client Authentication
        X509v3 Basic Constraints: CA:FALSE
```

---

## 4. Edge Cases & Security

| Issue | RFC Reference | Mitigation |
|-------|---------------|------------|
| Downgrade attack | §4.1.3, §E.1 | ServerHello random contains `DOWNGRD` (line ~1700). Our proxy MUST check this. |
| Overlong certificate chain | §4.4.2 | Limit chain to 3 certs (leaf + intermediate + root). |
| Unauthenticated 0-RTT | §2.3 | Replay risk. In interlink, 0-RTT is a SHOULD NOT — too dangerous for auth decisions. |
| PSK binder attack | §E.1 line 8034-8047 | MUST NOT combine external PSKs with cert-based auth unless negotiated by extension. |
| HelloRetryRequest | §4.1.4 | Must handle correctly. If client didn't offer shared group, server sends HRR. Our implementation MUST support this to avoid handshake failure. |
| Empty SNI | §4.2 (SNI ext) | Proxies connect to IPs, not hostnames. SNI is optional interlink. |
| CertificateRequest without client auth | §4.3.2 | If server doesn't send this, client doesn't authenticate. Our server MUST send it. Our client MUST reject if not received. |
| Side channel timing | §C.3 | Constant-time comparators for Finished HMAC and PSK binder. |

---

## 5. Test Vectors

RFC 8446 Appendix B defines the wire format. External test vectors:

- **RFC 8448** (separate RFC) — "Example Handshake Traces for TLS 1.3"
  - Full 1-RTT handshake, PSK resumption, 0-RTT
  - Located at: check local mirror (`/home/alex/rfcs/rfc8448.txt`)
- **BoringSSL's TLS 1.3 test vectors** (de facto standard)
- **rustls test suite** (our TLS library vendor)

---

## 6. Compliance Checklist (MUST/SHOULD/MAY)

### MUST
- [ ] Support TLS 1.3 only (no downgrade to 1.2)
- [ ] Implement TLS_AES_128_GCM_SHA256 (§9.1)
- [ ] Support x25519 key exchange (§4.2.7, line 2614)
- [ ] Verify ServerHello random does not contain downgrade sentinels (§4.1.3)
- [ ] Send CertificateRequest in every handshake (mTLS requirement)
- [ ] Validate peer's CertificateVerify signature over transcript hash
- [ ] Verify Finished message for key confirmation
- [ ] Abort handshake on unexpected message order (§4, line 1383)

### SHOULD
- [ ] Support TLS_CHACHA20_POLY1305_SHA256 (§9.1)
- [ ] Support secp256r1 as secondary ECDHE group (§4.2.7)
- [ ] Support Ed25519 signatures (§4.2.3)
- [ ] Support PSK resumption for proxy-to-proxy connection pooling (§2.2)
- [ ] Reject 0-RTT for authentication-required connections (§2.3)
- [ ] Implement HelloRetryRequest handling (§4.1.4)

### MAY
- [ ] Support secp384r1, secp521r1
- [ ] Support post-handshake client authentication (§4.6.2)
- [ ] Support raw public keys (RFC 7250) instead of X.509 certs (future optimization)

---

## 7. TDD Scaffold

```rust
// First test — handshake message ordering
#[test]
fn test_handshake_message_order() {
    // Section 4: "A peer which receives a handshake message in an
    // unexpected order MUST abort with 'unexpected_message' alert"
    let mut proxy = TlsProxy::new(test_config());
    assert!(proxy.receive_message(HandshakeType::ServerHello).is_err());
}

// Second test — certificate request is mandatory
#[test]
fn test_server_requests_client_cert() {
    let server = TlsServer::new(test_config());
    let msg = server.build_handshake();
    assert!(msg.contains(HandshakeType::CertificateRequest));
}

// Third test — SPIFFE ID extraction from SAN
#[test]
fn test_extract_spiffe_id() {
    let cert = build_test_cert("spiffe://trust/ns/foo/sa/bar");
    assert_eq!(extract_spiffe_id(&cert).unwrap(),
               SpiffeId::new("trust", "foo", "bar"));
}
```
