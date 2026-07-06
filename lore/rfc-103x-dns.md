# RFC 1034/1035 — Domain Name System (DNS)

**Sources:**
- RFC 1034 (STD 13): Domain Names — Concepts and Facilities
- RFC 1035 (STD 13): Domain Names — Implementation and Specification

**Status:** Draft
**Priority:** MEDIUM — used for service discovery and endpoint resolution

---

## 1. Summary

DNS is a hierarchical, distributed name resolution system. RFC 1034 describes the concepts (namespace, delegation, caching, resolution algorithm). RFC 1035 defines the wire format (message structure, resource records, compression). For interlink, DNS resolves service names to IP addresses, and SRV records (RFC 2782) locate specific ports. The proxy resolves upstream destinations via DNS rather than requiring a hardcoded endpoint list.

---

## 2. Key Sections

| Section | Lines | Content |
|---------|-------|---------|
| RFC 1034 §2 | Concepts | Name space, domain labels, fully qualified names |
| RFC 1034 §5 | Resolution | Iterative vs. recursive query algorithm |
| RFC 1034 §7 | Caching | TTL-based caching for performance |
| RFC 1035 §4.1 | Message format | Header (12 bytes) + Question + Answer + Authority + Additional |
| RFC 1035 §4.1.2 | Question section | QNAME, QTYPE, QCLASS |
| RFC 1035 §4.1.3 | RR format | NAME, TYPE, CLASS, TTL, RDLENGTH, RDATA |

---

## 3. Implementation Plan

```rust
pub struct ServiceDiscovery {
    resolver: trust_dns_resolver::TokioAsyncResolver,
    cache: DashMap<String, ResolvedEndpoints>,
    ttl: Duration,
}

pub struct ResolvedEndpoints {
    addrs: Vec<SocketAddr>,
    spiffe_id: Option<SpiffeId>,  // from SRV or k8s metadata
    ttl: Duration,
}

impl ServiceDiscovery {
    /// Resolve a service name to endpoints, with optional SRV lookup.
    /// Falls back from SRV to standard A/AAAA.
    pub async fn resolve(&self, name: &str) -> Result<ResolvedEndpoints> {
        // Try SRV first: _service._proto.name
        // Fall back to A/AAAA lookup
        // Cache with TTL
    }
}
```
