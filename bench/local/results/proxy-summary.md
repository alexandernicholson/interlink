# interlink proxy benchmark summary

Last run: 2026-07-08T14:45:34+09:00 @ bbba8c8
Server: Go echo-server (plain HTTP, 0 delay)
Proxy: interlinkd (inbound mTLS → plain TCP)
Load: Fortio HTTPS + mTLS

## proxy-interlink-bulk-256kb

Captured: 2026-07-08T14:49:37+09:00 @ bbba8c8
p50_ms=1.316 p90_ms=1.872 p99_ms=1.997 avg_ms=1.185 actual_qps=50.0 errors=0
avg_cpu_percent=6.11 peak_cpu_percent=6.50 avg_memory_rss_kb=14589.1 peak_memory_rss_kb=14808.0

## proxy-interlink-bulk-64kb

Captured: 2026-07-08T14:48:36+09:00 @ bbba8c8
p50_ms=0.629 p90_ms=0.954 p99_ms=1.649 avg_ms=0.552 actual_qps=100.0 errors=0
avg_cpu_percent=8.22 peak_cpu_percent=9.00 avg_memory_rss_kb=15454.4 peak_memory_rss_kb=15960.0

## proxy-interlink-churn-q100-c1

Captured: 2026-07-08T14:46:35+09:00 @ bbba8c8
p50_ms=0.861 p90_ms=1.451 p99_ms=1.947 avg_ms=0.880 actual_qps=100.0 errors=0
avg_cpu_percent=2.23 peak_cpu_percent=2.70 avg_memory_rss_kb=9722.1 peak_memory_rss_kb=9940.0

## proxy-interlink-churn-q500-c5

Captured: 2026-07-08T14:47:36+09:00 @ bbba8c8
p50_ms=1.089 p90_ms=1.819 p99_ms=1.984 avg_ms=1.052 actual_qps=500.0 errors=0
avg_cpu_percent=4.89 peak_cpu_percent=6.70 avg_memory_rss_kb=14143.5 peak_memory_rss_kb=15308.0


## Handshake metrics (final)
```
# TYPE interlink_handshakes_total counter
interlink_handshakes_total 36020
# TYPE interlink_handshake_resumed_total counter
interlink_handshake_resumed_total 0
# TYPE interlink_handshake_full_total counter
interlink_handshake_full_total 36020
```
