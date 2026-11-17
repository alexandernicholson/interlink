# Service Mesh Echo Capacity Comparison

Captured 2026-11-17T12:35:04Z; runtime source [`fb609291398e`](https://github.com/alexandernicholson/interlink/commit/fb609291398e4e41e7f3779ae231261e0dfc05ba). Actual harness, workload, manifest and build-input SHA256 hashes are recorded separately in the capture metadata.

Zero-delay HTTP echo, unlimited offered load; 20 s measured after 10 s warm-up per point. 5 s request timeout on every mesh. 1 KiB request/response with Fortio's fast client for RPS, 1 MiB request/response with its full-duplex standard client for payload throughput. Payload MiB/s is each direction, not doubled wire throughput.

Linux machine: x86_64, 4 logical CPUs, 15.3 GiB RAM, kernel 7.0.0-1014-aws. Client/server use separate workers in the same three-node kind cluster on this shared host. Interlink/Linkerd use sidecars; Istio ambient uses L4 ztunnel without a waypoint.

Pinned versions: kind v0.24.0, kubernetes v1.30.0, linkerd stable-2.14.10, istio 1.24.0, fortio 1.66.0.

Predeclared concurrency sweeps — rps: 1, 4, 16, 64, 256, 1024, 4096; bulk: 1, 4, 16, 64, 256, 1024. All points, including error boundaries, are retained.

| Mesh | Peak measured RPS (1 KiB) | p99 at RPS peak (ms) | Peak payload MiB/s each way (1 MiB) | Proxy CPU at RPS peak (m) | RSS at RPS peak (MiB) |
| --- | ---: | ---: | ---: | ---: | ---: |
| interlink | 37890.4 | 107.78 | 813.34 | 1455.3 | 256.2 |
| linkerd | 13624.4 | 34.96 | 281.66 | 1731.2 | 25.9 |
| istio-ambient | 25970.4 | 30.74 | 480.58 | 1711.2 | 17.3 |

## Selection and limits

- **interlink**: winning concurrency RPS 1024, bulk 16. Error boundaries: rps c4096: 274032/534908 measured errors, 140282 warm-up errors, bulk c1024: 177/13795 measured errors, 596 warm-up errors. Peaks are limited to the tested concurrency grid.
- **linkerd**: winning concurrency RPS 256, bulk 16. Error boundaries: bulk c1024: 2064/4750 measured errors, 273 warm-up errors. Peaks are limited to the tested concurrency grid.
- **istio-ambient**: winning concurrency RPS 256, bulk 4. Error boundaries: bulk c256: 8/9924 measured errors, 0 warm-up errors, bulk c1024: 3390/6398 measured errors, 1787 warm-up errors. Peaks are limited to the tested concurrency grid.

Only points with zero warm-up and measured request errors are eligible. Missing/invalid/stale metrics or incomplete grids fail the entire capture. Unlimited-load capacity testing may saturate the host; no fixed-target achievement or 85% host-CPU gate applies. Raw host CPU samples remain part of the evidence. CPU and actual RSS sum the proxy containers; they are not per-connection allocations.

These are peak measured results within this machine's declared sweep, not theoretical capacity or a guarantee for other machines, production networks, connection churn or TLS handshake throughput.

[Capture metadata and selected concurrency](capture.json) · [Run the comparison](../README.md)
