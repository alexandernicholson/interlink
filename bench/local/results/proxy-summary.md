# interlink proxy benchmark summary

Generated: 2026-07-08T13:03:18+09:00
Git SHA: 05c0a0a
Server: Go echo-server (plain HTTP, 0 delay)
Proxy: interlinkd (inbound mTLS → plain TCP)
Load: Fortio HTTPS + mTLS
Profiles: 60s each

## proxy-interlink-q320-c160

p50_ms=1.596 p90_ms=2.996 p99_ms=4.429 avg_ms=1.708 actual_qps=320.0 errors=0
avg_cpu_percent=0.83 peak_cpu_percent=1.40 avg_memory_rss_kb=14945.6 peak_memory_rss_kb=15476.0

## proxy-interlink-q3200-c1600

p50_ms=7.774 p90_ms=13.590 p99_ms=25.804 avg_ms=8.273 actual_qps=3198.0 errors=0
avg_cpu_percent=1.77 peak_cpu_percent=2.40 avg_memory_rss_kb=49635.2 peak_memory_rss_kb=53888.0

## proxy-interlink-q12800-c6400

p50_ms=23.598 p90_ms=33.842 p99_ms=63.731 avg_ms=23.911 actual_qps=12775.9 errors=0
avg_cpu_percent=5.50 peak_cpu_percent=7.10 avg_memory_rss_kb=165718.4 peak_memory_rss_kb=185168.0

## proxy-interlink-churn-q100-c1

p50_ms=1.194 p90_ms=1.841 p99_ms=1.987 avg_ms=1.078 actual_qps=100.0 errors=0
avg_cpu_percent=10.03 peak_cpu_percent=10.50 avg_memory_rss_kb=110949.1 peak_memory_rss_kb=203284.0

## proxy-interlink-churn-q500-c5

p50_ms=1.363 p90_ms=1.877 p99_ms=1.993 avg_ms=1.184 actual_qps=500.0 errors=0
avg_cpu_percent=9.25 peak_cpu_percent=9.60 avg_memory_rss_kb=65246.1 peak_memory_rss_kb=87076.0


## Handshake metrics (final)
```
# TYPE interlink_handshakes_total counter
interlink_handshakes_total 52332
# TYPE interlink_handshake_full_total counter
interlink_handshake_full_total 52332
# TYPE interlink_handshake_resumed_total counter
interlink_handshake_resumed_total 0
```
