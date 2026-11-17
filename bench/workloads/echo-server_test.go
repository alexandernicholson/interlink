package main

import (
	"bytes"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strconv"
	"testing"
	"time"
)

type duplexRecorder struct {
	*httptest.ResponseRecorder
}

func (recorder *duplexRecorder) EnableFullDuplex() error {
	return nil
}

func TestEchoPayloadHasExplicitLengthWithoutChunkFraming(t *testing.T) {
	for _, size := range []int{1024, 1048576} {
		t.Run(strconv.Itoa(size), func(t *testing.T) {
			server := httptest.NewServer(echoHandler(time.Nanosecond))
			defer server.Close()
			payload := bytes.Repeat([]byte{0, 127, 255, 42}, size/4)
			client := server.Client()
			client.Timeout = 3 * time.Second
			response, err := client.Post(server.URL, "application/octet-stream", bytes.NewReader(payload))
			if err != nil {
				t.Fatal(err)
			}
			defer response.Body.Close()
			body, err := io.ReadAll(response.Body)
			if err != nil {
				t.Fatal(err)
			}
			if response.StatusCode != http.StatusOK || !bytes.Equal(body, payload) {
				t.Fatalf("echo status=%d body bytes=%d, want identical %d-byte payload", response.StatusCode, len(body), size)
			}
			if response.Header.Get("Content-Length") != strconv.Itoa(size) || response.ContentLength != int64(size) {
				t.Fatalf("missing explicit payload length: headers=%v length=%d", response.Header, response.ContentLength)
			}
			if len(response.TransferEncoding) != 0 {
				t.Fatalf("echo added chunk framing: %v", response.TransferEncoding)
			}
		})
	}
}

type failedBody struct {
	closed bool
}

func (body *failedBody) Read([]byte) (int, error) {
	return 0, errors.New("injected request read failure")
}

func (body *failedBody) Close() error {
	body.closed = true
	return nil
}

func TestEchoRejectsUnreadableBodyAndClosesIt(t *testing.T) {
	body := &failedBody{}
	request := httptest.NewRequest(http.MethodPost, "/echo", body)
	request.ContentLength = 3
	response := &duplexRecorder{httptest.NewRecorder()}
	defer func() {
		if recovered := recover(); recovered != http.ErrAbortHandler || !body.closed {
			t.Fatalf("read failure abort=%v body closed=%t", recovered, body.closed)
		}
	}()
	echoHandler(0)(response, request)
	t.Fatal("unreadable echo body did not abort the response")
}

func TestEchoAbortsShortBodyEvenWhenReaderReturnsNoError(t *testing.T) {
	request := httptest.NewRequest(http.MethodPost, "/echo", bytes.NewReader([]byte("ab")))
	request.ContentLength = 3
	response := &duplexRecorder{httptest.NewRecorder()}
	defer func() {
		if recovered := recover(); recovered != http.ErrAbortHandler {
			t.Fatalf("short body did not abort: %v", recovered)
		}
	}()
	echoHandler(0)(response, request)
	t.Fatal("short echo body was accepted")
}

func TestEchoRejectsUnknownLengthAndUnsupportedDuplex(t *testing.T) {
	for _, length := range []int64{-1, 3} {
		request := httptest.NewRequest(http.MethodPost, "/echo", bytes.NewReader([]byte("abc")))
		request.ContentLength = length
		response := httptest.NewRecorder() // Deliberately has no EnableFullDuplex.
		echoHandler(0)(response, request)
		want := http.StatusInternalServerError
		if length < 0 {
			want = http.StatusLengthRequired
		}
		if response.Code != want {
			t.Fatalf("length=%d got status %d, want %d", length, response.Code, want)
		}
	}
}
