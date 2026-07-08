# interlink proxy benchmark summary

Generated: 2026-07-08T13:09:34+09:00
Git SHA: 885dad4
Server: Go echo-server (plain HTTP, 0 delay)
Proxy: interlinkd (inbound mTLS → plain TCP)
Load: Fortio HTTPS + mTLS
Profiles: 60s each

## proxy-interlink-q320-c160

p50_ms=1.606 p90_ms=3.051 p99_ms=3.969 avg_ms=1.730 actual_qps=320.0 errors=0
avg_cpu_percent=0.86 peak_cpu_percent=1.40 avg_memory_rss_kb=14837.3 peak_memory_rss_kb=15372.0

## proxy-interlink-q3200-c1600

p50_ms=7.231 p90_ms=12.219 p99_ms=27.974 avg_ms=7.809 actual_qps=3198.6 errors=0
avg_cpu_percent=1.73 peak_cpu_percent=2.30 avg_memory_rss_kb=49510.4 peak_memory_rss_kb=53844.0

## proxy-interlink-q12800-c6400

p50_ms=23.097 p90_ms=35.008 p99_ms=84.279 avg_ms=24.723 actual_qps=12773.7 errors=0
avg_cpu_percent=5.42 peak_cpu_percent=7.00 avg_memory_rss_kb=154795.2 peak_memory_rss_kb=172524.0


## Handshake metrics (final)
```
# TYPE interlink_handshake_resumed_total counter
interlink_handshake_resumed_total 0
# TYPE interlink_handshake_full_total counter
interlink_handshake_full_total 16320
# TYPE interlink_handshakes_total counter
interlink_handshakes_total 16320
```
