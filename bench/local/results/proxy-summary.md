# interlink proxy benchmark summary

Server: Go echo-server (plain HTTP, 200 ms delay)
Proxy: interlinkd (inbound mTLS → plain TCP)
Load: Fortio HTTPS + mTLS
Profiles: 60s each

## interlink-q320-c160

p50_ms=202.247 p90_ms=203.901 p99_ms=204.274 avg_ms=201.782 actual_qps=318.9 errors=0

## interlink-q3200-c1600

p50_ms=209.938 p90_ms=217.803 p99_ms=219.572 avg_ms=206.247 actual_qps=3188.7 errors=0

## interlink-q12800-c6400

p50_ms=225.077 p90_ms=245.037 p99_ms=249.528 avg_ms=217.875 actual_qps=12748.1 errors=0

