# Interlink — Ideation & Architecture

## Vision

A lightweight, RFC-first service mesh proxy written in Rust. Transparent mTLS, automatic HTTP/1.1 ↔ HTTP/2 proxying, zero-config identity, and minimal resource overhead. Designed to run on 1M devices from the cloud (64-core servers) to smartphones (ARM, 4-8 cores).

## Design Tenets

1. **RFC-first** — Every protocol decision is anchored to the relevant RFC. If it's not in the RFC, it's not in the spec.
2. **Minimal resource profile** — ≤10 MB RSS idle, ≤0.1 CPU core at baseline. Smartphone-friendly.
3. **Zero-config identity** — Derive identity from the environment (Linux cgroup, K8s service account, Android app ID). No deploy-time secrets.
4. **Transparent** — No code changes required in applications. iptables/nftables handles interception.
5. **Fail-closed** — If the proxy can't verify identity, the connection is rejected. No fallback to plaintext.
6. **SOLID** — Single-responsibility modules, open for extension, Liskov-substitutable protocol handlers, interface-segregated traits, dependency-injected components.

## RFC Dependency Map

```
interlink
├── mTLS Proxy ────── RFC 8446 (TLS 1.3)
│                    RFC 5280 (X.509 PKI)
│                    RFC 5746 (Renegotiation)
│                    RFC 7918 (False Start)
├── HTTP Detection ── RFC 9110 (HTTP Semantics)
│                    RFC 9112 (HTTP/1.1)
│                    RFC 9113 (HTTP/2)
│                    RFC 7540 (HTTP/2 prior)
├── Service Discovery ─ RFC 1034 (DNS Concepts)
│                       RFC 1035 (DNS Implementation)
│                       RFC 2782 (SRV records)
├── Identity ──────── SPIFFE spec (spiffe.io)
│                    RFC 5280 (X.509v3 SAN extension)
├── Proxy Injection ─ nftables/iptables (no RFC — Linux kernel ABI)
│                    (Conntrack: RFC 5382, RFC 4787)
├── Policy ────────── No single RFC; composes TLS auth results
├── Metrics ───────── No single RFC; OpenMetrics exposition format
└── TCP Proxy ─────── RFC 9293 (TCP — congestion, keepalive)
                     RFC 793 (original TCP spec)
```

## Component Architecture

```
┌─────────────────────────────────────────────────────────┐
│                   interlinkd (daemon)                     │
│                                                          │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌─────────┐ │
│  │ Identity │  │  mTLS    │  │Protocol  │  │Service  │ │
│  │ Provider │◄─┤ Handshake│──┤Detector  │──┤Discovery│ │
│  └────┬─────┘  └──────────┘  └────┬─────┘  └─────────┘ │
│       │                           │                      │
│  ┌────┴─────┐                ┌────┴─────┐               │
│  │ X.509 CA │                │ TCP Proxy│               │
│  │ (RFC5280)│                │ (RFC 9293)               │
│  └──────────┘                └──────────┘               │
│       │                           │                      │
│  ┌────┴─────┐  ┌──────────┐  ┌────┴─────┐               │
│  │  Cert    │  │  Policy  │  │iptables  │               │
│  │  Store   │  │  Engine  │  │Redirector│               │
│  └──────────┘  └──────────┘  └──────────┘               │
│       │                           │                      │
│  ┌────┴─────┐  ┌──────────┐                             │
│  │ Metrics  │  │  Config  │                             │
│  │ Exporter │  │  Watcher │                             │
│  └──────────┘  └──────────┘                             │
└─────────────────────────────────────────────────────────┘
```

## Data Plane Flow (per connection)

```
Inbound TCP ──► iptables REDIRECT ──► [4080] TCP Proxy
                                           │
                              ┌────────────┴────────────┐
                              │ Protocol Detection       │
                              │ (1st bytes heuristic)    │
                              ├─────┬──────┬──────┬─────┤
                              │HTTP1│HTTP2 │ gRPC │ Raw │
                              └──┬──┴──┬───┴──┬───┴──┬──┘
                                 │     │      │      │
                              ┌──┴──┐ ┌┴───┐ ┌┴───┐ ┌┴───┐
                              │mTLS │ │mTLS │ │mTLS │ │mTLS │
                              │Client│ │Client│ │Client│ │Client│
                              └──┬──┘ └──┬──┘ └──┬──┘ └──┬──┘
                                 │     │      │      │
                    ┌────────────┴─────┴──────┴──────┴──┐
                    │  Outbound TCP to upstream           │
                    └────────────────────────────────────┘
```

## Resource Budget (smartphone target)

| Metric     | Budget       | Notes                           |
|------------|-------------|----------------------------------|
| RSS memory | ≤8 MB idle   | Rust std alone is ~3 MB          |
|            | ≤20 MB active | Under 1000 conn/s load          |
| CPU        | ≤0.05 core idle | Epoll-based, no busy loops    |
|            | ≤0.3 core active | Crypto (x25519 + AES-256-GCM) |
| Binary     | ≤5 MB stripped | musl target, LTO, UPX optional |
| Startup    | ≤10 ms        | No heavy init; lazy-load CA     |

## Constraints

- **No `unsafe`** except in well-defined FFI boundaries (seccomp, iptables netlink).
- **No heap allocation in the hot path** (pre-allocate buffers per connection).
- **No standard library** dependency in protocol-critical paths (eventual `no_std` support for bare-metal? Unlikely but keep option open).
- **All config via environment variables + config file** — no CRDs, no Kubernetes API dependency. The proxy should work on bare Linux, Android (via termux), and containers alike.

## Module Interface Boundaries

Every module exposes a trait. Dependencies are injected at construction. No global state.

```rust
// Example: the mTLS handshake trait
#[async_trait]
pub trait TlsHandshake: Send + Sync {
    /// Perform a TLS 1.3 handshake as the client (outbound).
    async fn connect(&self, dst: SocketAddr, sni: &str) -> Result<TlsStream, Error>;

    /// Accept a TLS 1.3 handshake as the server (inbound).
    async fn accept(&self, stream: TcpStream) -> Result<TlsStream, Error>;
}
```

## RFC Reading Plan

Each major component has a `lore/rfc-NNNN-name.md` file containing:

1. **Summary** — What the RFC says in 3 paragraphs
2. **Key sections** — Direct line references into the local `/home/alex/rfcs/rfc*.txt` files
3. **Implementation plan** — Concrete Rust types, functions, and algorithms
4. **Edge cases** — Things the RFC warns about, security considerations
5. **Test vectors** — Where to find test data (RFC appendix or external)
6. **Compliance checklist** — MUST/SHOULD/MAY items extracted from the RFC

## TDD Workflow

1. Write failing test that expresses RFC requirement
2. Read the relevant section in `lore/rfc-*.md`
3. Implement minimal code to pass
4. Refactor to SOLID principles
5. Add benchmark
6. Commit
