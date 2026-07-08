# Architecture Overview

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

    style P1 fill:#4a9eff
    style P2 fill:#4a9eff
```

<hr />

## Connection Lifecycle (Inbound)

```mermaid
flowchart TD
    ACC[TCP Accept :4143] --> UP[Resolve upstream:<br/>default_upstream, else SO_ORIGINAL_DST<br/>with self-connect guard]
    UP --> SEM[Semaphore try_acquire<br/>max_connections]
    SEM --> TLS[TLS 1.3 Handshake<br/>mandatory client cert]
    TLS --> ALPN{ALPN negotiated<br/>il/mux/1?}
    ALPN -->|yes| MUXS[Serve yamux tunnel:<br/>each stream policy-checked,<br/>forwarded independently]
    ALPN -->|no| ID[Extract SPIFFE ID<br/>from cert SAN]
    ID --> POL[Policy Evaluation<br/>default-deny]
    POL -->|allow| PROTO[Protocol Detection<br/>HTTP/1.1 / HTTP/2 / TCP]
    POL -->|deny| CLOSE[Close connection]
    PROTO --> FWD[Forward to upstream<br/>+ bidirectional copy]
    MUXS --> FWD
    FWD --> DONE[Done]
```

## Connection Lifecycle (Outbound)

```mermaid
flowchart TD
    ACC[TCP Accept :4140] --> SO[SO_ORIGINAL_DST<br/>self-connect guarded,<br/>else default_upstream]
    SO --> SEM[Semaphore try_acquire<br/>max_connections]
    SEM --> DISC[ServiceDiscovery<br/>resolve host, then port]
    DISC --> ROUTE{Tunnel to peer<br/>in mux pool?}
    ROUTE -->|yes, under cap| STREAM[Open yamux stream<br/>no TLS handshake]
    ROUTE -->|no / at cap| TLS[TLS 1.3 Handshake<br/>SPIFFE server verification,<br/>ALPN offers il/mux/1]
    TLS -->|il/mux/1 negotiated| REG[Register tunnel in pool<br/>≤16 tunnels/peer, ~200 streams each]
    REG --> STREAM
    TLS -->|legacy peer| RELAY[1:1 relay]
    STREAM --> POL[Policy Evaluation<br/>default-deny]
    RELAY --> POL
    POL -->|allow| FWD[Bidirectional copy]
    POL -->|deny| CLOSE[Close connection]
    FWD --> DONE[Done]
```

<hr />

## Resource Budget

| Metric | Cloud Target | Edge/Smartphone Target |
|--------|-------------|----------------------|
| RSS memory | ≤20 MB active | ≤8 MB idle |
| CPU | ≤0.3 core active | ≤0.05 core idle |
| Binary size | ≤5 MB (stripped musl) | ≤5 MB |
| Startup time | ≤10 ms | ≤10 ms |
| Connections/sec | 10,000+ | 1,000+ |
