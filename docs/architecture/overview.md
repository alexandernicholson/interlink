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

    C1 -.->|plaintext TCP| P1
    P1 -->|inbound mTLS :4143| P2
    P1 -->|outbound mTLS :4140| P2
    P2 -.->|plaintext TCP| C2

    style P1 fill:#4a9eff
    style P2 fill:#4a9eff
```

<hr />

## Connection Lifecycle (Inbound)

```mermaid
flowchart TD
    ACC[TCP Accept :4143] --> SO[SO_ORIGINAL_DST<br/>recover upstream]
    SO --> SEM[Semaphore acquire<br/>max_connections]
    SEM --> TLS[TLS 1.3 Handshake<br/>mandatory client cert]
    TLS --> ID[Extract SPIFFE ID<br/>from cert SAN]
    ID --> POL[Policy Evaluation<br/>default-deny]
    POL -->|allow| PROTO[Protocol Detection<br/>HTTP/1.1 / HTTP/2 / TCP]
    POL -->|deny| CLOSE[Close connection]
    PROTO --> FWD[Forward to upstream<br/>+ bidirectional copy]
    FWD --> DONE[Done]
```

## Connection Lifecycle (Outbound)

```mermaid
flowchart TD
    ACC[TCP Accept :4140] --> SO[SO_ORIGINAL_DST<br/>recover real destination]
    SO --> SEM[Semaphore acquire<br/>max_connections]
    SEM --> DISC[ServiceDiscovery<br/>resolve upstream]
    DISC --> TLS[TLS 1.3 Handshake<br/>with client cert]
    TLS --> POL[Policy Evaluation<br/>default-deny]
    POL -->|allow| FWD[Forward to upstream<br/>+ bidirectional copy]
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
