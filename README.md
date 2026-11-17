# interlink

A lightweight service mesh proxy written in Rust: SPIFFE mTLS, default-deny authorization, and protocol detection.
[Veyrmux](https://github.com/alexandernicholson/veyrmux) lets application connections share authenticated proxy-to-proxy tunnels.

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

## Quick Start

Requires Rust 1.89 or later and a Unix-like system.
Run these commands from your [checkout](https://github.com/alexandernicholson/interlink):

```bash
# Generate local demo credentials; /tmp/interlink-demo must not exist.
TMPDIR=/tmp cargo run --example ca_bootstrap

# Terminal 1: start the mTLS echo backend.
cargo run --example backend -- /tmp/interlink-demo 8443

# Terminal 2: send a message over mTLS.
cargo run --example frontend -- /tmp/interlink-demo 127.0.0.1:8443
```

The frontend logs the backend identity and response:

```text
mTLS connected! Backend identity = spiffe://example.local/ns/default/sa/backend
Received: backend-echo[frontend]: Hello from frontend!
```

These examples use the TLS library directly. To run the proxy, follow the
[quickstart guide](docs/examples/quickstart.md).

### Run `interlinkd`

Provide an issued leaf certificate, its private key, and a trusted CA bundle.
Set `INTERLINK_IDENTITY` to the SPIFFE URI in the certificate.
Replace the identity and paths below with your issued credentials:

```bash
INTERLINK_TRUST_DOMAIN=example.local \
INTERLINK_IDENTITY=spiffe://example.local/ns/default/sa/proxy \
INTERLINK_CA_BUNDLE_PATH=/etc/interlink/ca-bundle.pem \
INTERLINK_CERT_PATH=/etc/interlink/tls.crt \
INTERLINK_KEY_PATH=/etc/interlink/tls.key \
cargo run --release --bin interlinkd
```

The daemon logs `interlinkd ready` after startup.
By default, it allows only peers with the same workload identity.
Configure an upstream before forwarding application traffic; see the
[quickstart guide](docs/examples/quickstart.md#running-the-daemon).

## Configuration

Environment variables override `/etc/interlink/config.json`.
Set `INTERLINK_CONFIG_FILE` to use another JSON file.

| Variable | Default | Purpose |
| --- | --- | --- |
| `INTERLINK_IDENTITY` | Required | SPIFFE identity in the leaf certificate. |
| `INTERLINK_TRUST_DOMAIN` | `cluster.local` | SPIFFE trust domain. |
| `INTERLINK_CA_BUNDLE_PATH` | Required | Trusted CA bundle, DER or PEM. |
| `INTERLINK_CERT_PATH` | Required | Leaf certificate, DER or PEM. |
| `INTERLINK_KEY_PATH` | Required | Private key, PKCS#8 DER or PEM. |
| `INTERLINK_PROXY_INBOUND_PORT` | `4143` | Peer mTLS listener. |
| `INTERLINK_PROXY_OUTBOUND_PORT` | `4140` | Local outbound listener; `0` disables it. |
| `INTERLINK_DEFAULT_UPSTREAM` | None | Destination without transparent interception. |
| `INTERLINK_METRICS_PORT` | `4190` | Prometheus metrics on loopback. |
| `INTERLINK_MUX` | `true` | Offer multiplexed tunnels. |

`POST /reload` on the default admin address, `127.0.0.1:4192`, reloads credentials for new handshakes.
Invalid credentials return `500`; active credentials and existing streams remain unchanged.
See [key management](docs/architecture/key-management.md) and
[production tuning](docs/production-tuning.md) for rotation and resource controls.

## Architecture

Each pod runs an interlink sidecar. See the [architecture overview](docs/architecture/overview.md)
and [key management](docs/architecture/key-management.md) for topologies and connection lifecycles.

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

The first connection to a peer pays one TLS handshake; later connections open streams on that tunnel:

```mermaid
sequenceDiagram
    participant A as Service A
    participant O as interlink (A's sidecar)
    participant I as interlink (B's sidecar)
    participant B as Service B

    A->>O: TCP connect, plaintext (outbound :4140)
    Note over O: Destination from SO_ORIGINAL_DST<br/>or configured upstream
    alt No reusable tunnel to this peer
        O->>I: TLS 1.3 ClientHello, ALPN il/mux/1 (inbound :4143)
        I->>O: Certificate (spiffe://…/sa/b)
        O->>I: Certificate (spiffe://…/sa/a)
        Note over O,I: Each side authorizes the peer identity (default-deny)
    end
    O->>I: Open Veyrmux stream
    Note over I: Authorize stream (default-deny)
    I->>B: TCP connect, plaintext
    A->>O: Request bytes
    O->>I: Stream data (encrypted)
    I->>B: Request bytes
    B-->>A: Response relayed back the same way
```

Veyrmux is wire-compatible with Yamux v0 under the `il/mux/1` ALPN; rolling-version tests cover
stock Yamux peers in both directions. `INTERLINK_MUX=false` removes the mux offer and peers fall back
to one-to-one relays, but a negotiated protocol is always honored.

## Benchmarks

Compare interlink, Linkerd, and Istio ambient under the same echo workload:

<!-- BEGIN GENERATED MESH HEADLINE -->
![Service mesh latency, memory, CPU and throughput](bench/results/comparison.svg)

All meshes at **10,000 RPS with 64 clients**; lines show capacity sweeps.

[Workload and results](bench/results/comparison.md) · [Run manually](bench/README.md)
<!-- END GENERATED MESH HEADLINE -->


## Documentation

- [Quickstart](docs/examples/quickstart.md) — install, run, and check the daemon.
- [Architecture](docs/architecture/overview.md) — deployment and connection flow.
- [Key management](docs/architecture/key-management.md) — credentials and rotation.
- [Production tuning](docs/production-tuning.md) — runtime controls and metrics.
- [Engineering rules](docs/engineering-rules.md) — requirements for contributors.

## License

Licensed under the [MIT License](LICENSE).
