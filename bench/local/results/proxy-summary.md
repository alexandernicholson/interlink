# interlink proxy benchmark summary

Server: Go echo-server (plain HTTP, 0 delay)
Proxy: interlinkd (inbound mTLS → plain TCP)
Load: Fortio HTTPS + mTLS
Profiles: 60s each

## proxy-interlink-q320-c160

p50_ms=1.514 p90_ms=2.868 p99_ms=3.966 avg_ms=1.609 actual_qps=320.0 errors=0
avg_cpu_percent=0.99 peak_cpu_percent=1.90 avg_memory_rss_kb=18934.4 peak_memory_rss_kb=20996.0

## proxy-interlink-q3200-c1600

p50_ms=8.079 p90_ms=12.340 p99_ms=20.513 avg_ms=8.176 actual_qps=3198.3 errors=0
avg_cpu_percent=2.03 peak_cpu_percent=2.60 avg_memory_rss_kb=58258.1 peak_memory_rss_kb=67340.0

## proxy-interlink-q12800-c6400

p50_ms=21.539 p90_ms=34.274 p99_ms=60.096 avg_ms=22.630 actual_qps=12753.5 errors=0
avg_cpu_percent=6.71 peak_cpu_percent=8.30 avg_memory_rss_kb=195130.9 peak_memory_rss_kb=233096.0

## proxy-interlink-bulk-64kb

p50_ms=0.690 p90_ms=1.225 p99_ms=1.762 avg_ms=0.656 actual_qps=100.0 errors=0
avg_cpu_percent=8.80 peak_cpu_percent=9.40 avg_memory_rss_kb=101704.5 peak_memory_rss_kb=202868.0

