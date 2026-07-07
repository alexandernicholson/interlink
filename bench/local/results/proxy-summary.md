# interlink proxy benchmark summary

Server: Go echo-server (plain HTTP, 200 ms delay)
Proxy: interlinkd (inbound mTLS → plain TCP)
Load: Fortio HTTPS + mTLS
Profiles: 60s each

## proxy-interlink-q320-c160

p50_ms=1.605 p90_ms=3.206 p99_ms=4.848 avg_ms=1.783 actual_qps=320.0 errors=0
avg_cpu_percent=0.98 peak_cpu_percent=2.20 avg_memory_rss_kb=20976.9 peak_memory_rss_kb=22768.0

## proxy-interlink-q3200-c1600

p50_ms=7.429 p90_ms=11.967 p99_ms=18.990 avg_ms=7.697 actual_qps=3199.4 errors=0
avg_cpu_percent=2.25 peak_cpu_percent=3.10 avg_memory_rss_kb=62206.8 peak_memory_rss_kb=78184.0

## proxy-interlink-q12800-c6400

p50_ms=22.523 p90_ms=34.681 p99_ms=71.292 avg_ms=23.647 actual_qps=12787.5 errors=0
avg_cpu_percent=5.30 peak_cpu_percent=6.70 avg_memory_rss_kb=212340.7 peak_memory_rss_kb=243368.0

