# interlink

**A lightweight service mesh proxy written in Rust.** SPIFFE mTLS, default-deny authorization, protocol detection, and native Tokio multiplexing with [Veyrmux](https://github.com/alexandernicholson/veyrmux). Many application connections share an authenticated proxy-to-proxy tunnel.

```mermaid
graph LR
    subgraph Cloud
        A[Service A<br/>Go/Node/Rust] --- P1[interlink proxy]
    end
    subgraph Edge
        B[Service B<br/>Python/Java] --- P2[interlink proxy]
    end
    P1 -- mTLS + Veyrmux streams --- P2
```

<hr />

## Quick Start

Requires Rust 1.89 or later.

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

```text
mTLS connected! Backend identity = spiffe://example.local/ns/default/sa/backend
Sent: Hello from frontend!
Received: backend-echo[frontend]: Hello from frontend!
```

### Run the `interlinkd` daemon

Use an issued leaf certificate, its private key, and a trusted CA bundle.
Set `INTERLINK_IDENTITY` to the SPIFFE URI in that leaf certificate:

```bash
INTERLINK_PROXY_INBOUND_PORT=4143 \
INTERLINK_PROXY_OUTBOUND_PORT=4140 \
INTERLINK_METRICS_PORT=4190 \
INTERLINK_TRUST_DOMAIN=example.local \
INTERLINK_IDENTITY=spiffe://example.local/ns/default/sa/proxy \
INTERLINK_CA_BUNDLE_PATH=/etc/interlink/ca-bundle.pem \
INTERLINK_CERT_PATH=/etc/interlink/tls.crt \
INTERLINK_KEY_PATH=/etc/interlink/tls.key \
cargo run --bin interlinkd
```

Replace the example identity and credential paths with your issued identity and
files. See [Configuration](#configuration) and the
[Quickstart Guide](docs/examples/quickstart.md) for deployment steps.

<hr />

## Configuration

For a step-by-step walkthrough, see the [Quickstart Guide](docs/examples/quickstart.md).

`interlinkd` loads configuration from two sources, in order of increasing precedence:

1. A JSON config file (default: `/etc/interlink/config.json`)
2. Environment variables prefixed with `INTERLINK_`

Example config file:

```json
{
  "trust_domain": "example.local",
  "identity": "spiffe://example.local/ns/default/sa/proxy",
  "proxy_inbound_port": 4143,
  "proxy_outbound_port": 4140,
  "metrics_port": 4190,
  "log_level": "info",
  "max_connections": 1024,
  "ca_bundle_path": "/etc/interlink/ca-bundle.pem",
  "cert_path": "/etc/interlink/tls.crt",
  "key_path": "/etc/interlink/tls.key"
}
```

Environment variable equivalents:

| Variable | Description | Default |
|----------|-------------|---------|
| `INTERLINK_PROXY_INBOUND_PORT` | Inbound transparent proxy port | 4143 |
| `INTERLINK_PROXY_OUTBOUND_PORT` | Outbound transparent proxy port (0 disables) | 4140 |
| `INTERLINK_METRICS_PORT` | Prometheus metrics port | 4190 |
| `INTERLINK_TRUST_DOMAIN` | SPIFFE trust domain | `cluster.local` |
| `INTERLINK_IDENTITY` | This proxy's SPIFFE ID; must match its leaf certificate | required |
| `INTERLINK_CA_BUNDLE_PATH` | CA bundle (DER or PEM) for peer validation | — |
| `INTERLINK_CERT_PATH` / `INTERLINK_KEY_PATH` | Leaf cert (DER or PEM) / key (PKCS#8 DER or PEM) | — |
| `INTERLINK_DEFAULT_UPSTREAM` | Fallback upstream when `SO_ORIGINAL_DST` is unavailable | — |
| `INTERLINK_MUX` | Offer multiplexed tunnels (ALPN `il/mux/1`) to peers | `true` |
| `INTERLINK_ACCEPTORS` | SO_REUSEPORT acceptor tasks per listener (1–16) | `max(2, min(available CPUs, 4))` |
| `INTERLINK_COPY_BUF_SIZE` | Relay copy buffer size in bytes (4 KiB–1 MiB) | `65536` |
| `INTERLINK_MAX_CONNECTIONS` | Per-proxy connection limit | 1024 |
| `INTERLINK_CONFIG_FILE` | Path to JSON config file | `/etc/interlink/config.json` |

`POST /reload` on the admin port atomically reloads the CA bundle, leaf certificate, and private key for new handshakes. A reload succeeds only when the files form a trusted, currently valid key pair with the configured SPIFFE identity; otherwise it returns `500` and preserves the last-known-good credentials. Existing TLS streams are uninterrupted.

For a Signet agent using its default shared-volume layout, point Interlink at the same files and set `INTERLINK_IDENTITY` to the SPIFFE ID derived from the agent's trust-domain, namespace, and service-account arguments:

```sh
INTERLINK_CA_BUNDLE_PATH=/var/run/interlink/ca-bundle.pem
INTERLINK_CERT_PATH=/var/run/interlink/identity/tls.crt
INTERLINK_KEY_PATH=/var/run/interlink/identity/tls.key
```

<hr />

## Architecture

For deployment topologies and detailed connection flows, see the [Architecture Overview](docs/architecture/overview.md) and [Key Management](docs/architecture/key-management.md) docs.

```mermaid
flowchart TB
    subgraph Proxy["interlink Proxy (per-pod sidecar)"]
        direction TB
        subgraph Inbound["Inbound (port 4143)"]
            L[TCP Listener] --> H[TLS 1.3 + SPIFFE authentication]
            H --> M{ALPN<br/>il/mux/1?}
            M -- yes --> Y[Veyrmux logical streams]
            M -- no --> I[Legacy TCP connection]
            Y --> P[Authorize each stream<br/>default-deny]
            I --> P
            P --> F[Forward to upstream]
        end
        subgraph Outbound["Outbound (port 4140)"]
            O[TCP Listener] --> OM[Upstream destination]
            OM --> OT{Reusable tunnel<br/>under admission limits?}
            OT -- yes --> OS[Open Veyrmux stream]
            OT -- no --> OH[TLS 1.3 + SPIFFE authentication]
            OH --> NEG{ALPN<br/>il/mux/1?}
            NEG -- yes --> POOL[Register Veyrmux tunnel]
            POOL --> OS
            NEG -- no --> LEG[Legacy 1:1 relay]
            OS --> OF[Authorized application relay]
            LEG --> OF
        end
        subgraph Admin["Admin (port 4192)"]
            AD[HTTP Server] --> AH["/healthz /readyz /reload"]
        end
    end
    Peer[Peer interlink<br/>proxy] -- mTLS --> L
    Client[Application<br/>Container] -- plaintext --> O
    F --> App[Local App<br/>Container]
    OF -- mTLS --> RemotePeer[Peer interlink<br/>proxy]
```

Veyrmux uses Tokio I/O directly and remains wire-compatible with Yamux v0 under
the unchanged `il/mux/1` ALPN. Rolling-version tests cover stock Yamux peers in
both directions. Stock Yamux and the Tokio/Futures compatibility adapter are
test-only dependencies.

`INTERLINK_MUX=false` disables the multiplexing offer; peers without mux use
one-to-one relays. A negotiated protocol is always honored.

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
| [RFC 9293](https://www.rfc-editor.org/rfc/rfc9293.html) | TCP | Connection management, congestion control |

<hr />

## Project Structure

```text
interlink/
├── Cargo.toml              # Dependencies (tokio, rustls, veyrmux, rcgen, hickory-resolver, metrics)
├── README.md               # This file
├── docs/                   # Documentation
│   ├── architecture/       # Deployment topologies, network flows
│   ├── rfcs/               # RFC summaries and compliance
│   └── examples/           # Example walkthroughs
├── bench/                  # Service-mesh comparison harness
│   └── results/comparison.md # Sole published performance comparison
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
│   │   ├── mux.rs          # Native Tokio Veyrmux tunnels, Yamux-v0 wire format, per-peer pool
│   │   ├── verify.rs       # SpiffeServerVerifier: RFC 5280 chain + SPIFFE identity (no RFC 6125 name match)
│   │   ├── original_dst.rs # SO_ORIGINAL_DST helper (optional transparent mode, self-connect guarded)
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
│   ├── mux_tunnel.rs            # Mux tunnels: 1 handshake for N conns, pool past 512 streams, fallback
│   ├── spiffe_verify.rs         # SPIFFE server verification: identity accept, untrusted-CA/wrong-domain reject
│   ├── halfclose_propagation.rs # FIN/close_notify propagation through the relay
│   └── proxy_shutdown.rs        # Accept-loop shutdown + bind-failure handling
├── test-support/           # Shared mTLS integration-test fixtures
└── benches/
    ├── proxy.rs            # Protocol, identity, policy and relay decisions
    ├── runtime.rs          # DNS, config, metrics and runtime services
    ├── crypto.rs           # TLS/certificate and cryptographic operations
    ├── data_plane.rs       # Real two-proxy mux/legacy connection lifecycles
    └── path_inventory.json # Declared semantic cases and locked source-coverage limits
```

<hr />

## Test Suite

```bash
./scripts/preflight.sh        # Strict clippy, tests and semantic-path coverage gate
cargo test                    # Unit and integration suites
bash scripts/demo.sh          # End-to-end mTLS demo
```

| Suite | What it covers |
|-------|----------------|
| Unit | Validated identity, compiled policy, CA, gated DNS single-flight, mux coordination, relay progress, metrics and config |
| Integration | Real mTLS, resumption, concurrent mux streams, native/stock-Yamux rolling interoperability, exact bytes and EOF, rejection/reset isolation, half-close, credential reload and shutdown |

<hr />

## Benchmarks

The [service-mesh comparison](bench/results/comparison.md) is the only published
performance report. It compares interlink, Linkerd, and Istio ambient using matched
HTTP load profiles. Read its capture date, source revision, and workload limits
before applying the results to your deployment.

To collect a complete dataset, follow the [benchmark guide](bench/README.md).
Internal semantic-path checks remain correctness gates, not additional published
performance comparisons.

<hr />

## License

Licensed under the [MIT License](LICENSE).

<hr />

## References

- [SPIFFE](https://spiffe.io) — Secure Production Identity Framework for Everyone
- [IETF RFC Editor](https://www.rfc-editor.org) — All referenced RFCs
- [Linkerd](https://linkerd.io) — Inspiration for the sidecar proxy pattern
- [rustls](https://github.com/rustls/rustls) — TLS library in Rust
- [rcgen](https://github.com/rustls/rcgen) — X.509 certificate generation
