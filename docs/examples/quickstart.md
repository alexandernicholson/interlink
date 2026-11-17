# Run interlink locally

Try an mTLS echo exchange, then start `interlinkd` with workload credentials.

```mermaid
flowchart LR
    F[Frontend example] -->|mTLS| B[Backend example]
```

The examples use the TLS library directly, not the daemon's forwarding path.

## Prerequisites

- Rust 1.89 or later with Cargo.
- A Unix-like system; Linux is required for optional `SO_ORIGINAL_DST` interception.
- Git to clone the repository and `curl` to check readiness.

## 1. Install

```bash
git clone https://github.com/alexandernicholson/interlink.git
cd interlink
cargo build --release
```

The daemon binary is `target/release/interlinkd`.

## 2. Generate demo credentials

```bash
TMPDIR=/tmp cargo run --example ca_bootstrap
```

This creates `/tmp/interlink-demo` with CA, backend, and frontend credentials.
The directory must not already exist; the generator refuses to overwrite credentials.
These credentials are for local testing only.

## 3. Run the echo exchange

In one terminal, start the backend:

```bash
cargo run --example backend -- /tmp/interlink-demo 8443
```

Wait for `Backend listening on port 8443`. In another terminal, from the same checkout, run:

```bash
cargo run --example frontend -- /tmp/interlink-demo 127.0.0.1:8443
```

Verify the frontend logs the backend identity and echoed message:

```text
mTLS connected! Backend identity = spiffe://example.local/ns/default/sa/backend
Received: backend-echo[frontend]: Hello from frontend!
```

Stop the backend with Ctrl+C when finished.

## Running the Daemon

Provide a CA bundle, an issued leaf certificate, and its matching private key.
Set the trust domain and SPIFFE identity to match the certificate.
For local testing, reuse the backend credentials generated above:

```bash
INTERLINK_TRUST_DOMAIN=example.local \
INTERLINK_IDENTITY=spiffe://example.local/ns/default/sa/backend \
INTERLINK_CA_BUNDLE_PATH=/tmp/interlink-demo/ca.der \
INTERLINK_CERT_PATH=/tmp/interlink-demo/server.der \
INTERLINK_KEY_PATH=/tmp/interlink-demo/server.key \
INTERLINK_DEFAULT_UPSTREAM=127.0.0.1:8080 \
./target/release/interlinkd
```

This configures inbound forwarding to a plaintext application on `127.0.0.1:8080`.
Start your application there before sending traffic.
The daemon accepts peer mTLS on port `4143` and local outbound TCP on `4140`.
The fixed upstream applies to both proxy directions; use separate configurations for local-app and remote-peer forwarding.
The default policy permits only peers with the same workload identity.

The daemon logs `interlinkd ready`. Check readiness in another terminal:

```bash
curl --fail http://127.0.0.1:4192/readyz
```

Expect HTTP `200` when ready.
Configuration uses `INTERLINK_` environment variables over an optional JSON file at `/etc/interlink/config.json`;
set `INTERLINK_CONFIG_FILE` to select another file.

## Questions

**Why does startup report `INTERLINK_IDENTITY must be set`?**
Set the SPIFFE URI from your leaf certificate along with all three credential paths.

**Why does certificate generation fail with `create private demo dir`?**
The demo directory already exists or cannot be created. Remove old demo credentials only after stopping processes that use them, then regenerate.

**Why does the frontend reject `localhost:8443`?**
The example requires an IP socket address. Use `127.0.0.1:8443`, which the generated certificate supports.

**Can I rotate daemon credentials without stopping existing streams?**
Yes. Replace the credential files and call `POST /reload` on the admin address.
Invalid credentials return `500` and leave active credentials unchanged.

## Next steps

See [configuration](../../README.md#configuration), [deployment architecture](../architecture/overview.md),
and [key management](../architecture/key-management.md) for a deployment beyond the local demo.
