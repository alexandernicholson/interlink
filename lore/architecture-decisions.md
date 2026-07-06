# Architecture Decision Records (ADRs)

## ADR-0001: Rust as the Sole Implementation Language

**Context:** We need a runtime that can scale from cloud servers (64 cores, GBs of RAM) to smartphones (4-8 ARM cores, <100MB free RAM). C++/Go/Rust were considered.

**Decision:** Use Rust with `no_std`-compatible crate choices.

**Consequences:**
- Zero-cost abstractions, no GC pause
- Cross-compilation to ARM/aarch64 is mature
- MUSL target produces fully static 5MB binaries
- Trade-off: async ecosystem is younger than Go's

---

## ADR-0002: No CRL or OCSP — Expiry IS Revocation

**Context:** RFC 5280 mandates CRLs or OCSP for certificate revocation. Implementing OCSP responders or CRL distribution points adds significant operational complexity.

**Decision:** Issue 24-hour TTL certificates. Expiry replaces revocation.

**Consequences:**
- No CRL distribution points or OCSP responders needed
- Certificate validation is purely local (signature + expiry check)
- Clock skew tolerance: 1-hour window
- Max exposure window for compromised key: 24 hours
- Trade-off: services must refresh their cert every 24h (done by the identity agent)

---

## ADR-0003: x25519 + Ed25519 as Primary Crypto

**Context:** RFC 8446 §4.2.7 lists supported groups; §4.2.3 lists signature algorithms.

**Decision:** x25519 for key exchange, Ed25519 for signatures. AES-128-GCM and ChaCha20-Poly1305 for AEAD.

**Consequences:**
- x25519 is fast on all architectures (no dedicated hardware needed)
- Ed25519 signatures are small (64 bytes) and verify quickly
- ChaCha20-Poly1305 preferred on ARM (no AES-NI)
- AES-128-GSM available for x86 with AES-NI
- Trade-off: x25519 curves are not FIPS-compliant (not relevant for our use case)

---

## ADR-0004: No Protocol Conversion (HTTP/1.1 ↔ HTTP/2)

**Context:** Many proxies convert between HTTP versions for optimization. This introduces complexity and bugs.

**Decision:** The proxy preserves the original protocol version. HTTP/1.1 stays HTTP/1.1 through the mesh. HTTP/2 stays HTTP/2.

**Consequences:**
- Simpler code, fewer edge cases
- No HPACK↔HTTP/1.1 header translation
- Clients and servers negotiate their own protocol
- Trade-off: mixed-version services require the client to know the server's capability

---

## ADR-0005: Default-Deny Authorization Policy

**Context:** Service meshes must prevent unauthorized access. Default-allow is dangerous; default-deny is safe but requires explicit configuration.

**Decision:** Default-deny. All traffic is blocked unless explicitly allowed by a policy rule.

**Consequences:**
- Zero-trust networking: every connection must be authorized
- Namespace-scoped rules for easy onboarding
- Global rules for cross-cutting concerns
- Requires explicit policy configuration at deploy time
- Trade-off: initial setup requires more config than default-allow

---

## ADR-0006: Unix Domain Socket for Workload API (instead of HTTP)

**Context:** The identity agent needs to provide credentials to local workloads. Options: HTTP API on localhost, or Unix domain socket.

**Decision:** Unix domain socket at `/run/interlink/sockets/agent.sock`.

**Consequences:**
- No network exposure of credentials
- Kernel-level authentication (peer credentials via `SO_PEERCRED`)
- No port conflicts
- Trade-off: only works on the same host (not a problem — this is local-only)
- Trade-off: requires filesystem access to the socket path

---

## ADR-0007: Iptables REDIRECT for Transparent Proxy (not TPROXY)

**Context:** Linux provides two mechanisms for transparent proxying: REDIRECT (DNAT to local port) and TPROXY (transparent proxy socket option).

**Decision:** Use iptables REDIRECT for inbound, iptables DNAT for outbound.

**Consequences:**
- REDIRECT preserves original destination via `SO_ORIGINAL_DST`
- Works with all versions of iptables/nftables
- No need for `ip rule` or advanced routing
- Trade-off: TPROXY would preserve original source IP, which REDIRECT changes to localhost

---

## ADR-0008: LazyInit Pattern for Hot Start

**Context:** The proxy should start accepting connections within 10ms, even if the CA/identity system isn't ready yet.

**Decision:** Use `std::sync::LazyLock` for the CA trust bundle; use `oneshot` channels with timeout for identity bootstrap.

**Consequences:**
- Proxy binds port within milliseconds
- First connection will wait for identity (up to 5s timeout)
- No crash on startup if K8s API/DNS is slow
- Trade-off: first connection latency is higher
