# interlink proxy benchmark summary

Generated: 2026-07-08T13:19:35+09:00
Git SHA: e49cf59
Server: Go echo-server (plain HTTP, 0 delay)
Proxy: interlinkd (inbound mTLS → plain TCP)
Load: Fortio HTTPS + mTLS
Profiles: 60s each

## proxy-interlink-q320-c160

p50_ms=1.661 p90_ms=3.537 p99_ms=4.784 avg_ms=1.853 actual_qps=319.9 errors=0
avg_cpu_percent=0.81 peak_cpu_percent=1.40 avg_memory_rss_kb=14709.9 peak_memory_rss_kb=15220.0

## proxy-interlink-q3200-c1600

p50_ms=7.182 p90_ms=12.861 p99_ms=23.665 avg_ms=7.716 actual_qps=3198.6 errors=0
avg_cpu_percent=1.81 peak_cpu_percent=2.40 avg_memory_rss_kb=51312.0 peak_memory_rss_kb=55144.0

## proxy-interlink-q12800-c6400

p50_ms=24.166 p90_ms=37.382 p99_ms=72.708 avg_ms=25.338 actual_qps=12774.1 errors=0
avg_cpu_percent=5.61 peak_cpu_percent=7.30 avg_memory_rss_kb=175126.1 peak_memory_rss_kb=203956.0


## Handshake metrics (final)
```
# TYPE interlink_handshake_resumed_total counter
interlink_handshake_resumed_total 0
# TYPE interlink_handshakes_total counter
interlink_handshakes_total 15395
# TYPE interlink_handshake_full_total counter
interlink_handshake_full_total 15395
```
