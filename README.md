# interlink

**A lightweight, RFC-first service mesh proxy written in Rust.** Zero-config mTLS, automatic protocol detection, policy-based authorization, and ALPN-negotiated multiplexed tunnels that amortize the TLS handshake across all connections to a peer (−63 % proxy CPU under connection churn). Designed for 1M devices — from cloud servers to smartphones.

```mermaid
graph LR
    subgraph Cloud
        A[Service A<br/>Go/Node/Rust] --- P1[interlink proxy]
    end
    subgraph Edge
        B[Service B<br/>Python/Java] --- P2[interlink proxy]
    end
    P1 -- mTLS encrypted --- P2
    style P1 fill:#4a9eff,stroke:#2a6fcc
    style P2 fill:#4a9eff,stroke:#2a6fcc
```

<hr />

## Quick Start

### Example programs

```bash
# Generate certificates
cargo run --example ca_bootstrap

# Terminal 1: start backend
cargo run --example backend -- /tmp/interlink-demo 8443

# Terminal 2: connect frontend
cargo run --example frontend -- /tmp/interlink-demo localhost:8443
```

Expected output:

```
mTLS connected! Backend identity = spiffe://example.local/ns/default/sa/backend
Sent: Hello from frontend!
Received: backend-echo[frontend]: Hello from frontend!
```

### Run the `interlinkd` daemon

```bash
# Run with defaults (reads env vars and an optional config file)
cargo run --bin interlinkd

# Override specific settings
INTERLINK_INBOUND_PORT=4143 \
INTERLINK_OUTBOUND_PORT=4140 \
INTERLINK_ADMIN_PORT=4192 \
INTERLINK_CA_CERT=/etc/interlink/ca.der \
INTERLINK_CA_KEY=/etc/interlink/ca.key \
cargo run --bin interlinkd
```

See [Configuration](#configuration) and the full [Quickstart Guide](docs/examples/quickstart.md) for the full set of options.

<hr />

## Configuration

For a step-by-step walkthrough, see the [Quickstart Guide](docs/examples/quickstart.md).

`interlinkd` loads configuration from two sources, in order of increasing precedence:

1. A JSON config file (default: `/etc/interlink/interlink.json`)
2. Environment variables prefixed with `INTERLINK_`

Example config file:

```json
{
  "inbound_port": 4143,
  "outbound_port": 4140,
  "admin_port": 4192,
  "ca_cert_path": "/etc/interlink/ca.der",
  "ca_key_path": "/etc/interlink/ca.key",
  "node_name": "worker-01",
  "cluster_domain": "cluster.local",
  "metrics_enabled": true
}
```

Environment variable equivalents:

| Variable | Description | Default |
|----------|-------------|---------|
| `INTERLINK_PROXY_INBOUND_PORT` | Inbound transparent proxy port | 4143 |
| `INTERLINK_PROXY_OUTBOUND_PORT` | Outbound transparent proxy port (0 disables) | 4140 |
| `INTERLINK_METRICS_PORT` | Prometheus metrics port | 4190 |
| `INTERLINK_TRUST_DOMAIN` | SPIFFE trust domain | `cluster.local` |
| `INTERLINK_IDENTITY` | This proxy's SPIFFE ID | derived |
| `INTERLINK_CA_BUNDLE_PATH` | CA bundle (DER) for peer validation | — |
| `INTERLINK_CERT_PATH` / `INTERLINK_KEY_PATH` | Leaf cert (DER) / key (PKCS#8 DER) | — |
| `INTERLINK_DEFAULT_UPSTREAM` | Fallback upstream when `SO_ORIGINAL_DST` is unavailable | — |
| `INTERLINK_MUX` | Offer multiplexed tunnels (ALPN `il/mux/1`) to peers | `true` |
| `INTERLINK_ACCEPTORS` | SO_REUSEPORT acceptor tasks per listener (1–16) | `min(cores,4)` |
| `INTERLINK_COPY_BUF_SIZE` | Relay copy buffer size in bytes (4 KiB–1 MiB) | `65536` |
| `INTERLINK_MAX_CONNECTIONS` | Per-proxy connection limit | 1024 |
| `INTERLINK_CONFIG_FILE` | Path to JSON config file | `/etc/interlink/config.json` |

Runtime reload of policy and certificate settings is available via `POST /reload` on the admin port.

<hr />

## Architecture

For deployment topologies and detailed connection flows, see the [Architecture Overview](docs/architecture/overview.md) and [Key Management](docs/architecture/key-management.md) docs.

```mermaid
flowchart TB
    subgraph Proxy["interlink Proxy (per-pod sidecar)"]
        direction TB
        subgraph Inbound["Inbound (port 4143)"]
            L[TCP Listener] --> A[iptables REDIRECT]
            A --> H[TLS 1.3 Handshake<br/>RFC 8446]
            H --> I[Identity Extraction<br/>RFC 5280 SAN]
            I --> P[Policy Engine<br/>default-deny]
            P --> D[Protocol Detection<br/>HTTP/1.1 / HTTP/2 / TCP]
            D --> F[Forward to Upstream]
        end
        subgraph Outbound["Outbound (port 4140)"]
            O[TCP Listener] --> OM[Orig. Dst Lookup]
            OM --> OT{Mux tunnel<br/>to peer?}
            OT -- yes --> OS[Open stream<br/>no handshake]
            OT -- no --> OH[TLS 1.3 Handshake<br/>ALPN il/mux/1 + client cert]
            OH --> OF[Tunnel or 1:1 relay]
            OS --> OF
        end
        subgraph Admin["Admin (port 4192)"]
            AD[HTTP Server] --> AH["/healthz /readyz /reload"]
        end
    end
    Client[Application<br/>Container] --> L
    Client --> O
    F --> Upstream[Upstream<br/>Service]
    OF --> Upstream
    style Proxy fill:#1a1a2e,stroke:#4a9eff
```

<hr />

```mermaid
sequenceDiagram
    participant C as Client App
    participant P as interlink Proxy
    participant U as Upstream Service

    Note over C,P: TCP connection (iptables redirect)
    C->>+P: TCP SYN
    P-->>-C: SYN-ACK

    Note over C,P: mTLS Handshake (RFC 8446 §2)
    C->>P: ClientHello + key_share
    P->>C: ServerHello + key_share + CertificateRequest
    P->>C: Certificate (spiffe://trust/ns/default/sa/proxy)
    P->>C: CertificateVerify + Finished
    C->>P: Certificate (spiffe://trust/ns/default/sa/client)
    C->>P: CertificateVerify + Finished

    Note over C,P: Encrypted tunnel established
    C->>P: GET /api HTTP/1.1 (encrypted)

    Note over P,U: Forward to upstream
    P->>U: GET /api HTTP/1.1
    U->>P: HTTP 200 OK (response)
    P->>C: HTTP 200 OK (encrypted)

    Note over C,P: Policy: allow
```

<hr />

## RFC References

Detailed RFC compliance notes live in [`docs/rfcs/`](docs/rfcs/).

| RFC | Title | Usage |
|-----|-------|-------|
| [RFC 8446](docs/rfcs/rfc-8446-tls1.3.md) | TLS 1.3 | mTLS handshake, cipher suites, key exchange |
| [RFC 5280](docs/rfcs/rfc-5280-x509.md) | X.509 PKI | Certificate profiles, SAN encoding, path validation |
| [RFC 9110](docs/rfcs/rfc-911x-http.md) | HTTP Semantics | Method tokens, status codes, header handling |
| [RFC 9112](docs/rfcs/rfc-911x-http.md) | HTTP/1.1 | Request-line format, persistent connections |
| [RFC 9113](docs/rfcs/rfc-911x-http.md) | HTTP/2 | Frame format, multiplexing, connection preface |
| [RFC 1034](docs/rfcs/rfc-103x-dns.md) | DNS Concepts | Hierarchical namespace, resolution algorithm |
| [RFC 1035](docs/rfcs/rfc-103x-dns.md) | DNS Implementation | Message format, resource records |
| [RFC 9293](docs/rfcs/rfc-8446-tls1.3.md) | TCP | Connection management, congestion control |

<hr />

## Project Structure

```
interlink/
├── Cargo.toml              # Dependencies (tokio, rustls, ring, rcgen, hickory-resolver, metrics, serde_json)
├── README.md               # This file
├── lore/                   # Architecture decisions & RFC plans
│   ├── ideation.md         # Component breakdown, resource budget
│   ├── architecture-decisions.md  # ADR-0001 through ADR-0008
│   ├── rfc-8446-tls1.3.md  # TLS 1.3 implementation plan
│   ├── rfc-5280-x509.md    # X.509 PKI implementation plan
│   ├── rfc-911x-http.md    # HTTP/1.1 + HTTP/2 plan
│   └── rfc-103x-dns.md     # DNS implementation plan
├── docs/                   # Documentation
│   ├── architecture/       # Deployment topologies, network flows
│   ├── rfcs/               # RFC summaries and compliance
│   ├── examples/           # Example walkthroughs
│   └── benchmarks/         # Performance data
├── src/
│   ├── lib.rs              # Module root
│   ├── common/             # Identity types, errors, constants
│   │   ├── identity.rs     # SpiffeId, TrustDomain, IdentityProvider
│   │   ├── error.rs        # InterlinkError enum
│   │   ├── constants.rs    # Ports, timeouts, buffer sizes
│   │   └── config.rs       # Runtime configuration
│   ├── identity/
│   │   ├── ca/mod.rs       # CertificateAuthority (Ed25519, rcgen)
│   │   └── provider/       # K8s/environment identity providers
│   ├── proxy/
│   │   ├── handshake.rs    # TlsHandshake trait, TlsClient, TlsServer
│   │   ├── tcp.rs          # Inbound TcpProxy (accept, mTLS, forward, copy)
│   │   ├── outbound.rs     # Outbound proxy: mux tunnel pool + 1:1 fallback
│   │   ├── mux.rs          # Multiplexed mTLS tunnels (yamux over ALPN il/mux/1)
│   │   ├── original_dst.rs # SO_ORIGINAL_DST helper for transparent redirect
│   │   └── config.rs       # ProxyConfig
│   ├── admin/mod.rs        # Admin HTTP server (/healthz, /readyz, /reload)
│   ├── protocol/mod.rs     # ProtocolDetector (H1, H2, TCP)
│   ├── discovery/dns.rs    # ServiceDiscovery (hickory-resolver)
│   ├── policy/mod.rs       # PolicyEngine (default-deny, wildcards)
│   └── metrics/mod.rs      # Prometheus exporter, atomic counters
├── examples/
│   ├── ca_bootstrap.rs     # Generate CA + certs
│   ├── backend.rs          # mTLS echo server
│   └── frontend.rs         # mTLS client
├── scripts/
│   └── demo.sh             # End-to-end demo runner
├── tests/
│   ├── e2e_proxy_test.rs        # Full mTLS handshake + echo through proxy
│   ├── mtls_handshake.rs        # SPIFFE, policy, TLS-resumption tests
│   ├── mux_tunnel.rs            # Mux tunnels: 1 handshake for N conns, fallback
│   ├── halfclose_propagation.rs # FIN/close_notify propagation through the relay
│   └── proxy_shutdown.rs        # Accept-loop shutdown + bind-failure handling
└── benches/
    ├── proxy.rs             # Protocol detection, SPIFFE parse benchmarks
    └── crypto.rs            # Ed25519 sign/verify, X25519 keygen
```

<hr />

## Test Suite

```bash
./scripts/preflight.sh        # clippy -D warnings + full test suite (pre-commit gate)
cargo test                    # 81 tests across unit + integration suites
cargo bench                   # Micro-benchmarks
bash scripts/demo.sh          # End-to-end mTLS demo
```

| Suite | What it covers |
|-------|----------------|
| Unit (56) | SpiffeId, compiled policy patterns, CA, DNS single-flight (gated-fake concurrency), metrics, config |
| E2E (25) | mTLS handshake + echo through the proxy, TLS 1.3 resumption (`Full`→`Resumed`), mux tunnels (1 handshake for N connections, concurrent streams, legacy-peer fallback), half-close propagation, shutdown + bind-failure handling |

<hr />

## Benchmarks

### Micro-benchmarks (Linux x86_64, 16 cores)

See [`docs/benchmarks/results.md`](docs/benchmarks/results.md) for methodology.

```
policy_engine/evaluate       time:   [40.5 ns]   (compiled patterns; was 8.2 µs string-matched)
pattern_match/compiled       time:   [25.9 ns]
protocol_detection/http1.1   time:   [~12 ns]
spiffe_id/parse              time:   [~49 ns]
```

Hot-path numbers that matter more than microbenches: TLS 1.3 session resumption is
verified end-to-end (`Full` → `Resumed`), and multiplexed tunnels carry ~19,000
connections over a single TLS handshake (measured: −63 % combined proxy CPU and −46 %
p50 latency at 500 conns/s churn versus per-connection handshakes — see
`docs/performance-plan.md`).

### Service mesh comparison

A reproducible benchmark harness lives in [`bench/`](bench/). All meshes share one
apples-to-apples topology: a **3-node kind cluster** (`bench/manifests/kind-3node.yaml`)
with the load generator and the echo server pinned to separate worker nodes so mesh
traffic always crosses the node boundary, identical workload (Go echo server, 200 ms
fixed delay, 1 KB payload, Fortio), and CPU/memory sampled the same way
(`kubectl top pod`, summed across each mesh's proxy pods).

```bash
cd bench
./setup.sh                                  # pinned kind/kubectl/linkerd/istioctl
./scripts/run-interlink.sh                  # interlink (mux tunnels)
INTERLINK_MUX=false ./scripts/run-interlink.sh   # interlink (1:1 control)
./scripts/run-linkerd.sh
./scripts/run-istio.sh
python3 scripts/aggregate.py                # -> bench/results/comparison.md
```

The refreshed comparison table is regenerated by `aggregate.py` after a full run; see
[`bench/results/comparison.md`](bench/results/comparison.md) for the latest published
numbers and [`lore/benchmark-status.md`](lore/benchmark-status.md) for methodology notes
and current measurement caveats.

Independently measured on the reproducible single-proxy harness (see
`docs/performance-plan.md`): compiled policy evaluation at **40.5 ns**, verified TLS 1.3
resumption, and multiplexed tunnels carrying **~19,000 connections over a single
handshake** (−63 % proxy CPU) under connection churn.

<hr />

## License

Committed to the public domain. See `UNLICENSE` or `LICENSE` file.

<hr />

## References

- [SPIFFE](https://spiffe.io) — Secure Production Identity Framework for Everyone
- [IETF RFC Editor](https://www.rfc-editor.org) — All referenced RFCs
- [Linkerd](https://linkerd.io) — Inspiration for the sidecar proxy pattern
- [rustls](https://github.com/rustls/rustls) — TLS library in Rust
- [rcgen](https://github.com/rustls/rcgen) — X.509 certificate generation
