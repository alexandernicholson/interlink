# RFC 8446 — TLS 1.3

**Status:** Implemented  
**Source:** [RFC 8446](https://www.rfc-editor.org/info/rfc8446)  
**Local copy:** `/home/alex/rfcs/rfc8446.txt` (8963 lines)

## Abstract

TLS 1.3 provides a secure channel between two peers with server authentication (mandatory) and client authentication (optional — but **mandatory for interlink**). Uses (EC)DHE key exchange for forward secrecy, AEAD ciphers for encryption, HKDF for key derivation. All handshake messages after ServerHello are encrypted.

## Key Sections

| Section | Lines | Content |
|---------|-------|---------|
| 1 Introduction | 287-349 | Auth, confidentiality, integrity |
| 1.2 Major Differences | 405-472 | Removed static RSA, DHE, compression |
| 2 Protocol Overview | 511-715 | Full handshake diagram (Figure 1) |
| 2.2 Resumption/PSK | 791-916 | PSK-based resumption |
| 4 Handshake Protocol | 1317-1400 | Message ordering |
| 4.1.1 Crypto Negotiation | 1407-1483 | Cipher suites, groups, signatures |
| 4.2.3 Signature Algorithms | 2300-2400 | Ed25519, ECDSA, RSA-PSS |
| 4.2.7 Supported Groups | 2600-2662 | x25519 (MUST), secp256r1 |
| 4.4.2 Certificate | ~3200-3350 | X.509v3 with SPIFFE SAN |
| 9.1 Mandatory Ciphers | — | TLS_AES_128_GCM_SHA256 |

## Compliance

### MUST (implemented)
- TLS 1.3 only (no downgrade to 1.2)
- TLS_AES_128_GCM_SHA256 cipher suite
- x25519 key exchange
- ServerHello downgrade sentinel verification
- CertificateRequest in every handshake (mTLS)
- CertificateVerify signature verification
- Finished message verification
- ALPN negotiation (`h2`, `http/1.1`) — RFC 7301
- Per-connection TLS handshake timeout (`timeouts::TLS_HANDSHAKE`)

### SHOULD (implemented)
- TLS_CHACHA20_POLY1305_SHA256 (ARM-optimized)
- secp256r1 secondary group
- Ed25519 signatures

### MAY (not yet)
- PSK resumption for connection pooling
- Post-handshake client authentication
