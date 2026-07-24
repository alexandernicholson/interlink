# Quickstart Guide

## Prerequisites

- Rust 1.75+ (edition 2021)
- Linux (only for the optional `SO_ORIGINAL_DST` transparent-interception mode; the standard sidecar deployment needs no iptables and no special privileges)
- `cargo` installed

## Installation

```bash
git clone <repo> interlink
cd interlink
cargo build --release
```

## Running the Daemon

The `interlinkd` binary is a per-pod sidecar mTLS proxy. It terminates inbound mTLS from peer proxies (port 4143 by default) and forwards plaintext to the local app; applications send outbound traffic to port 4140, which it carries to the peer's sidecar over mTLS (multiplexed tunnels when the peer supports ALPN `il/mux/1`). An admin server runs on port 4192. Where the destination isn't configured via `INTERLINK_DEFAULT_UPSTREAM`, the outbound listener can recover it transparently via `SO_ORIGINAL_DST`.

```bash
# Run with defaults; optionally create /etc/interlink/config.json first
cargo run --bin interlinkd

# With an explicit config file (there are no CLI flags; configuration is
# env vars plus an optional JSON file)
INTERLINK_CONFIG_FILE=/etc/interlink/config.json cargo run --bin interlinkd
```

Admin endpoints:

- `GET /healthz` — liveness probe
- `GET /readyz` — readiness probe
- `POST /reload` — atomically reload the CA bundle, certificate, and private key for new handshakes; invalid files return `500` without replacing the active credentials

## Running the Demo

The demo shows two services (frontend + backend) communicating through mTLS using interlink's `TlsServer` and `TlsClient`.

### Step 1: Generate Certificates

```bash
cargo run --example ca_bootstrap
```

Output:

```
CA + certs written to: /tmp/interlink-demo
  ca.der       — root CA certificate
  server.der   — backend service cert
  server.key   — backend service private key
  client.der   — frontend service cert
  client.key   — frontend service private key
```

### Step 2: Start Backend

```bash
cargo run --example backend -- /tmp/interlink-demo 8443
```

Listens on port 8443, accepts mTLS connections, echoes received data with identity information.

### Step 3: Connect Frontend

```bash
cargo run --example frontend -- /tmp/interlink-demo localhost:8443
```

Connects to backend via mTLS, presents client certificate, sends message, receives echo.

### Automated Demo

```bash
bash scripts/demo.sh
```

## Using interlink as a Library

```toml
[dependencies]
interlink = { git = "<repo>" }
```

```rust
use interlink::identity::ca::CertificateAuthority;
use interlink::common::identity::SpiffeId;
use interlink::proxy::handshake::{TlsServer, TlsClient, TlsHandshake};
```

### Generate a Certificate

```rust
let ca = CertificateAuthority::new("mycluster.local")?;
let id = SpiffeId::new("mycluster.local", "default", "my-service");
let (cert, key) = ca.issue_leaf_with_key(&id, &["localhost"])?;
```

### Create a TLS Server

```rust
let provider = MyIdentityProvider::new();
let server = Arc::new(TlsServer::new(
    Arc::new(provider),
    cert,
    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
)?);
let tls_stream = server.accept(tcp_stream).await?;
println!("Peer identity: {}", tls_stream.peer_identity);
```

### Create a TLS Client

```rust
let client = TlsClient::with_client_auth(
    Arc::new(provider),
    client_cert,
    client_key,
)?;
let tls_stream = client.connect("localhost:8443").await?;
println!("Server identity: {}", tls_stream.peer_identity);
```
