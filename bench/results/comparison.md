# Service Mesh Echo Capacity Comparison

Captured 2026-11-17T12:02:38Z; runtime source [`205d05b2851b`](https://github.com/alexandernicholson/interlink/commit/205d05b2851b4422b39a0e116d75dff6c91ba649). Actual harness, workload, manifest and build-input SHA256 hashes are recorded separately in the capture metadata.

Zero-delay HTTP echo, unlimited offered load; 20 s measured after 10 s warm-up per point. 5 s request timeout on every mesh. 1 KiB request/response with Fortio's fast client for RPS, 1 MiB request/response with its full-duplex standard client for payload throughput. Payload MiB/s is each direction, not doubled wire throughput.

Linux machine: x86_64, 8 logical CPUs, 3.3 GiB RAM, kernel 7.0.0-29-generic. Client/server use separate workers in the same three-node kind cluster on this shared host. Interlink/Linkerd use sidecars; Istio ambient uses L4 ztunnel without a waypoint.

Pinned versions: kind v0.24.0, kubernetes v1.30.0, linkerd stable-2.14.10, istio 1.24.0, fortio 1.66.0.

Predeclared concurrency sweeps — rps: 1, 4, 16, 64, 256, 1024, 4096; bulk: 1, 4, 16, 64, 256, 1024. All points, including error boundaries, are retained.

| Mesh | Peak measured RPS (1 KiB) | p99 at RPS peak (ms) | Peak payload MiB/s each way (1 MiB) | Proxy CPU at RPS peak (m) | RSS at RPS peak (MiB) |
| --- | ---: | ---: | ---: | ---: | ---: |
| interlink | 69856.4 | 68.95 | 2272.86 | 2203.1 | 76.4 |
| linkerd | 19000.9 | 6.34 | 426.81 | 1872.0 | 12.1 |
| istio-ambient | 60045.6 | 65.02 | 1025.53 | 2515.0 | 46.4 |

## Selection and limits

- **interlink**: winning concurrency RPS 1024, bulk 64. Error boundaries: rps c4096: 346740/1075654 measured errors, 175571 warm-up errors, bulk c1024: 211421/213187 measured errors, 134493 warm-up errors. Peaks are limited to the tested concurrency grid.
- **linkerd**: winning concurrency RPS 64, bulk 16. Error boundaries: bulk c1024: 4096/4096 measured errors, 2078 warm-up errors. Peaks are limited to the tested concurrency grid.
- **istio-ambient**: winning concurrency RPS 1024, bulk 256. Error boundaries: bulk c1024: 4035/4155 measured errors, 2012 warm-up errors. Peaks are limited to the tested concurrency grid.

Only points with zero warm-up and measured request errors are eligible. Missing/invalid/stale metrics or incomplete grids fail the entire capture. Unlimited-load capacity testing may saturate the host; no fixed-target achievement or 85% host-CPU gate applies. Raw host CPU samples remain part of the evidence. CPU and actual RSS sum the proxy containers; they are not per-connection allocations.

These are peak measured results within this machine's declared sweep, not theoretical capacity or a guarantee for other machines, production networks, connection churn or TLS handshake throughput.

[Capture metadata and selected concurrency](capture.json) · [Run the comparison](../README.md)
