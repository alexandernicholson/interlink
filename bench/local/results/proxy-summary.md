# interlink proxy benchmark summary

Server: Go echo-server (plain HTTP, 0 delay)
Proxy: interlinkd (inbound mTLS → plain TCP)
Load: Fortio HTTPS + mTLS
Profiles: 60s each

## proxy-interlink-q320-c160

p50_ms=1.593 p90_ms=2.941 p99_ms=3.927 avg_ms=1.703 actual_qps=320.0 errors=0
avg_cpu_percent=0.86 peak_cpu_percent=1.40 avg_memory_rss_kb=14740.0 peak_memory_rss_kb=15264.0

## proxy-interlink-q3200-c1600

p50_ms=7.058 p90_ms=13.955 p99_ms=24.930 avg_ms=7.880 actual_qps=3198.6 errors=0
avg_cpu_percent=1.76 peak_cpu_percent=2.30 avg_memory_rss_kb=49802.1 peak_memory_rss_kb=54160.0

## proxy-interlink-q12800-c6400

p50_ms=22.804 p90_ms=33.830 p99_ms=44.326 avg_ms=23.100 actual_qps=12778.9 errors=0
avg_cpu_percent=5.78 peak_cpu_percent=7.40 avg_memory_rss_kb=187230.4 peak_memory_rss_kb=215864.0

## proxy-interlink-churn-q100-c1

p50_ms=0.948 p90_ms=1.736 p99_ms=1.960 avg_ms=0.985 actual_qps=100.0 errors=0
avg_cpu_percent=8.15 peak_cpu_percent=8.60 avg_memory_rss_kb=102587.5 peak_memory_rss_kb=187264.0

## proxy-interlink-churn-q500-c5

p50_ms=1.309 p90_ms=1.867 p99_ms=1.993 avg_ms=1.153 actual_qps=500.0 errors=0
avg_cpu_percent=7.67 peak_cpu_percent=8.20 avg_memory_rss_kb=60586.7 peak_memory_rss_kb=80072.0

