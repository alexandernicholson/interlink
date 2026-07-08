# interlink proxy benchmark summary

Generated: 2026-07-08T13:34:40+09:00
Git SHA: 6355caa
Server: Go echo-server (plain HTTP, 0 delay)
Proxy: interlinkd (inbound mTLS → plain TCP)
Load: Fortio HTTPS + mTLS
Profiles: 60s each

## proxy-interlink-q320-c160

p50_ms=1.492 p90_ms=2.956 p99_ms=3.917 avg_ms=1.626 actual_qps=320.0 errors=0
avg_cpu_percent=0.83 peak_cpu_percent=1.70 avg_memory_rss_kb=14690.9 peak_memory_rss_kb=15200.0

## proxy-interlink-q3200-c1600

p50_ms=7.652 p90_ms=12.450 p99_ms=20.316 avg_ms=7.942 actual_qps=3198.6 errors=0
avg_cpu_percent=1.81 peak_cpu_percent=2.40 avg_memory_rss_kb=51049.9 peak_memory_rss_kb=55852.0

## proxy-interlink-q12800-c6400

p50_ms=21.808 p90_ms=32.602 p99_ms=48.472 avg_ms=22.101 actual_qps=12776.3 errors=0
avg_cpu_percent=5.64 peak_cpu_percent=7.20 avg_memory_rss_kb=182101.3 peak_memory_rss_kb=214584.0

## proxy-interlink-bulk-64kb

p50_ms=0.691 p90_ms=1.237 p99_ms=1.926 avg_ms=0.652 actual_qps=100.0 errors=0
avg_cpu_percent=7.78 peak_cpu_percent=8.30 avg_memory_rss_kb=83867.7 peak_memory_rss_kb=197116.0

