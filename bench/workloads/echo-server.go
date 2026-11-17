package main

import (
	"flag"
	"io"
	"log"
	"net/http"
	"strconv"
	"time"
)

func echoHandler(delay time.Duration) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		defer r.Body.Close()
		if r.ContentLength < 0 {
			http.Error(w, "echo requires Content-Length", http.StatusLengthRequired)
			return
		}
		// HTTP/1 otherwise consumes unread request bytes before writing a
		// response. The standard controller permits streaming the same body
		// back without allocating a request-sized buffer.
		if err := http.NewResponseController(w).EnableFullDuplex(); err != nil {
			http.Error(w, "full-duplex echo unavailable", http.StatusInternalServerError)
			return
		}
		if delay > 0 {
			time.Sleep(delay)
		}
		w.Header().Set("Content-Type", "application/octet-stream")
		// Fortio's fast client counts response headers in Sizes. Explicit
		// framing makes Sizes - HeaderSizes the echoed application bytes,
		// even for bulk responses that net/http would otherwise chunk.
		w.Header().Set("Content-Length", strconv.FormatInt(r.ContentLength, 10))
		w.WriteHeader(http.StatusOK)
		if copied, err := io.Copy(w, r.Body); err != nil || copied != r.ContentLength {
			// Headers are committed. net/http's documented sentinel aborts
			// the response instead of presenting a truncated echo as success.
			panic(http.ErrAbortHandler)
		}
	}
}

func main() {
	addr := flag.String("addr", ":8080", "listen address")
	delay := flag.Duration("delay", 0, "fixed processing delay")
	flag.Parse()

	http.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("ok"))
	})

	http.HandleFunc("/echo", echoHandler(*delay))

	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, "/echo", http.StatusFound)
	})

	log.Printf("echo-server listening on %s with delay %v", *addr, *delay)
	if err := http.ListenAndServe(*addr, nil); err != nil {
		log.Fatalf("server error: %v", err)
	}
}
