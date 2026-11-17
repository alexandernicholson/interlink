# Architecture Overview

The proxy-to-proxy transport uses native Tokio
[Veyrmux](https://github.com/alexandernicholson/veyrmux), with Yamux-v0 wire
compatibility under ALPN `il/mux/1`. Existing stock-Yamux peers remain compatible;
peers without mux negotiation use one-to-one relays.

## Deployment Topology

```mermaid
flowchart LR
    subgraph K8s_Node["Kubernetes Node"]
        subgraph Pod_A["Pod: frontend"]
            C1[App Container<br/>frontend:3000]
            P1[interlink sidecar]
        end
        subgraph Pod_B["Pod: backend"]
            C2[App Container<br/>backend:8080]
            P2[interlink sidecar]
        end
    end

    C1 -.->|plaintext TCP to outbound :4140| P1
    P1 -->|mTLS, ALPN il/mux/1, to inbound :4143| P2
    P2 -.->|plaintext TCP| C2

```

<hr />

## Connection Lifecycle (Inbound)

```mermaid
flowchart TD
    ACC[TCP Accept :4143] --> SEM[Try admission permit<br/>max_connections]
    SEM --> TLS[TLS 1.3 mutual authentication<br/>SPIFFE peer identity]
    TLS --> ALPN{ALPN negotiated<br/>il/mux/1?}
    ALPN -->|yes| MUXS[Veyrmux logical streams]
    ALPN -->|no| LEGACY[Legacy TCP connection]
    MUXS --> POL[Authorize each application stream<br/>default-deny]
    LEGACY --> POL
    POL -->|deny| CLOSE[Close connection or reset stream]
    POL -->|allow| UP[Resolve and dial upstream<br/>self-connect and egress guards]
    UP --> FWD[Bidirectional relay<br/>independent half-close propagation]
    FWD --> DONE[Application connection complete]
```

## Connection Lifecycle (Outbound)

```mermaid
flowchart TD
    ACC[TCP Accept :4140] --> SEM[Try admission permit<br/>max_connections]
    SEM --> SO[Upstream destination<br/>SO_ORIGINAL_DST with self-connect guard<br/>or configured fallback]
    SO --> DISC[Service discovery when needed]
    DISC --> ROUTE{Reusable tunnel<br/>with admission capacity?}
    ROUTE -->|yes| STREAM[Open Veyrmux stream<br/>no new TLS handshake]
    ROUTE -->|no| TLS[TLS 1.3 + SPIFFE authentication<br/>offer il/mux/1 when enabled]
    TLS -->|il/mux/1 negotiated| REG[Register Veyrmux tunnel<br/>configured stream and pool limits]
    REG --> STREAM
    TLS -->|no mux negotiated| RELAY[Legacy 1:1 relay]
    STREAM --> POL[Policy Evaluation<br/>default-deny]
    RELAY --> POL
    POL -->|allow| FWD[Bidirectional copy]
    POL -->|deny| CLOSE[Close connection]
    FWD --> DONE[Done]
```

One logical stream carries one application TCP connection. Reusing a tunnel does
not reuse a finished application stream. Authorization and stream admission
remain per connection; half-close on one direction does not discard reverse
traffic.

`INTERLINK_MUX=false` removes the mux offer, not support for speaking a negotiated
protocol. Stock Yamux and the Tokio/Futures adapter are test-only dependencies;
production relay I/O stays native Tokio.

<hr />

