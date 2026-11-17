# Service mesh benchmarks

![Latency, memory, CPU and throughput](comparison.svg)

All meshes at **10,000 RPS with 64 clients**; lines show capacity sweeps.

Zero-delay HTTP echo: 1 KiB requests/responses; bulk uses 1 MiB each way.
One Linux host: 8 logical CPUs, 3.3 GiB RAM; client and server on separate kind workers. Interlink/Linkerd use sidecars; Istio ambient uses L4 ztunnel.

20 s per capacity point after 10 s warm-up; 5 s request timeout. Error-free measurements only.

[Run the benchmarks](../README.md)
