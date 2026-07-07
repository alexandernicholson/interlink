# interlink proxy benchmark summary

Server: Go echo-server (plain HTTP, 200 ms delay)
Proxy: interlinkd (inbound mTLS → plain TCP)
Load: Fortio HTTPS + mTLS
Profiles: 60s each

## proxy-interlink-q320-c160

p50_ms=1.630 p90_ms=3.081 p99_ms=4.283 avg_ms=1.758 actual_qps=320.0 errors=0
avg_cpu_percent=1.00 peak_cpu_percent=1.90 avg_memory_rss_kb=19546.7 peak_memory_rss_kb=21060.0

## proxy-interlink-q3200-c1600

p50_ms=7.275 p90_ms=12.863 p99_ms=23.664 avg_ms=7.941 actual_qps=3198.4 errors=0
avg_cpu_percent=1.99 peak_cpu_percent=2.60 avg_memory_rss_kb=56162.7 peak_memory_rss_kb=64204.0

## proxy-interlink-q12800-c6400

p50_ms=23.648 p90_ms=35.148 p99_ms=62.672 avg_ms=24.459 actual_qps=12779.0 errors=0
avg_cpu_percent=6.21 peak_cpu_percent=7.90 avg_memory_rss_kb=175703.7 peak_memory_rss_kb=217900.0

## proxy-interlink-churn-q100-c1

p50_ms=0.923 p90_ms=1.679 p99_ms=1.972 avg_ms=0.958 actual_qps=100.0 errors=0
avg_cpu_percent=8.65 peak_cpu_percent=8.90 avg_memory_rss_kb=101267.2 peak_memory_rss_kb=182584.0

## proxy-interlink-churn-q500-c5

p50_ms=1.377 p90_ms=1.883 p99_ms=1.997 avg_ms=1.185 actual_qps=500.0 errors=0
avg_cpu_percent=8.67 peak_cpu_percent=9.50 avg_memory_rss_kb=66860.8 peak_memory_rss_kb=87524.0

