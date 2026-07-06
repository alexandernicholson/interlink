# RFC 9110/9112/9113 — HTTP

## Sources

| RFC | Title | Status |
|-----|-------|--------|
| RFC 9110 | HTTP Semantics (STD 97) | Implemented |
| RFC 9112 | HTTP/1.1 (STD 99) | Implemented |
| RFC 9113 | HTTP/2 (STD 98) | Implemented |

## Protocol Detection

The proxy detects HTTP protocol from the first bytes of plaintext (post-TLS termination):

| Pattern | Protocol | RFC Reference |
|---------|----------|---------------|
| `PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n` | HTTP/2 | RFC 9113 §3.4 |
| `GET/POST/PUT/DELETE/HEAD/PATCH/OPTIONS/TRACE/CONNECT` | HTTP/1.1 | RFC 9112 §3 |
| Other | Raw TCP | — |

## HTTP/2 Frames (RFC 9113 §4.1)

```
HTTP/2 Frame Header: 9 bytes
+-----------------------------------------------+
| Length (24)          | Type (8) | Flags (8)   |
+-----------------------------------------------+
| R (1) | Stream Identifier (31)                 |
+-----------------------------------------------+
| Frame Payload (0..variable)                    |
+-----------------------------------------------+
```

Stream states (RFC 9113 §5.1): idle → open → half-closed → closed.
