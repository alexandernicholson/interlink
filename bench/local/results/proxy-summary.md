# interlink proxy benchmark summary

Server: Go echo-server (plain HTTP, 0 delay)
Proxy: interlinkd (inbound mTLS → plain TCP)
Load: Fortio HTTPS + mTLS
Profiles: 60s each

## proxy-interlink-q320-c160

p50_ms=1.450 p90_ms=2.943 p99_ms=3.961 avg_ms=1.570 actual_qps=320.0 errors=0
avg_cpu_percent=0.84 peak_cpu_percent=1.40 avg_memory_rss_kb=14669.9 peak_memory_rss_kb=15176.0

## proxy-interlink-q3200-c1600

p50_ms=7.282 p90_ms=12.934 p99_ms=23.073 avg_ms=7.875 actual_qps=3198.5 errors=0
avg_cpu_percent=1.73 peak_cpu_percent=2.30 avg_memory_rss_kb=51656.8 peak_memory_rss_kb=56204.0

## proxy-interlink-q12800-c6400

p50_ms=25.046 p90_ms=35.793 p99_ms=73.511 avg_ms=25.649 actual_qps=12759.3 errors=0
avg_cpu_percent=5.71 peak_cpu_percent=7.30 avg_memory_rss_kb=180418.7 peak_memory_rss_kb=210256.0

## proxy-interlink-bulk-64kb

p50_ms=0.683 p90_ms=1.294 p99_ms=1.934 avg_ms=0.664 actual_qps=100.0 errors=0
avg_cpu_percent=8.05 peak_cpu_percent=8.60 avg_memory_rss_kb=97122.9 peak_memory_rss_kb=201248.0

