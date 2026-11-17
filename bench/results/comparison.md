# Service Mesh Benchmark Comparison

Captured 2026-11-17T08:47:24Z. Interlink revision [`2bf373967de9`](https://github.com/alexandernicholson/interlink/commit/2bf373967de96f235ea9836ca0764aa479975fdf).

HTTP echo: 1,024 B payload, 200 ms application delay, 60 s measured per profile. Client and server use separate workers in the same three-node kind layout. Interlink and Linkerd use sidecars; Istio ambient uses node-level ztunnel.

Pinned versions: kind v0.24.0, kubernetes v1.30.0, linkerd stable-2.14.10, istio 1.24.0, fortio 1.66.0.

| Mesh | Target RPS | Achieved RPS | p50 ms | p99 ms | Proxy CPU (m) | Memory (MiB) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| interlink | 320 | 318.9 | 201.89 | 203.26 | 16.4 | 23.8 |
| interlink | 800 | 797.3 | 225.19 | 249.71 | 31.9 | 31.8 |
| interlink | 1600 | 1594.5 | 204.03 | 207.67 | 56.1 | 53.5 |
| linkerd | 320 | 318.9 | 204.11 | 207.12 | 42.7 | 13.0 |
| linkerd | 800 | 797.1 | 208.94 | 216.59 | 92.4 | 23.4 |
| linkerd | 1600 | 1594.1 | 213.88 | 226.20 | 190.1 | 41.0 |
| istio-ambient | 320 | 318.9 | 202.92 | 205.27 | 47.4 | 6.5 |
| istio-ambient | 800 | 797.3 | 204.63 | 208.65 | 87.2 | 9.7 |
| istio-ambient | 1600 | 1594.5 | 207.81 | 215.12 | 140.3 | 14.6 |

Only complete matched captures with zero request errors, at least 97% of target RPS and host CPU below 85% are published. CPU and RSS sum the proxy containers using kubelet statistics; these are not per-connection allocations.

[Capture metadata](capture.json) · [Run the comparison](../README.md)
