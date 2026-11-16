# interlink proxy benchmark summary

Last run: 2026-11-16T17:19:05+09:00 @ 14207fd
Server: Go echo-server (native, plain HTTP, 0 delay)
Load: Fortio HTTPS + mTLS (native, pinned 1.66.0)
Proxy: interlinkd (inbound mTLS → plain TCP)

## proxy-interlink-bulk-256kb

Captured: 2026-11-16T17:25:05+09:00 @ 14207fd
p50_ms=2.225 p90_ms=3.746 p99_ms=7.600 avg_ms=2.427 actual_qps=50.0 errors=0
avg_cpu_percent=3.62 peak_cpu_percent=4.60 avg_memory_rss_kb=21673.6 peak_memory_rss_kb=21760.0

## proxy-interlink-bulk-64kb

Captured: 2026-11-16T17:23:35+09:00 @ 14207fd
p50_ms=0.956 p90_ms=1.896 p99_ms=4.125 avg_ms=1.124 actual_qps=100.0 errors=0
avg_cpu_percent=3.69 peak_cpu_percent=29.80 avg_memory_rss_kb=21243.2 peak_memory_rss_kb=21280.0

## proxy-interlink-churn-q100-c1

Captured: 2026-11-16T17:20:35+09:00 @ 14207fd
p50_ms=2.600 p90_ms=4.573 p99_ms=11.875 avg_ms=3.045 actual_qps=100.0 errors=0
avg_cpu_percent=7.48 peak_cpu_percent=9.70 avg_memory_rss_kb=13780.8 peak_memory_rss_kb=14736.0

## proxy-interlink-churn-q500-c5

Captured: 2026-11-16T17:22:05+09:00 @ 14207fd
p50_ms=3.712 p90_ms=6.677 p99_ms=16.522 avg_ms=4.383 actual_qps=500.0 errors=0
avg_cpu_percent=22.75 peak_cpu_percent=25.40 avg_memory_rss_kb=19053.3 peak_memory_rss_kb=20128.0


## Handshake metrics (final)
```
# TYPE interlink_handshakes_total counter
interlink_handshakes_total 54020
# TYPE interlink_handshake_resumed_total counter
interlink_handshake_resumed_total 0
# TYPE interlink_handshake_full_total counter
interlink_handshake_full_total 54020
```
