FROM golang:1.23-alpine AS builder
WORKDIR /src
COPY echo-server.go go.mod ./
RUN go mod init echo-server 2>/dev/null || true
RUN CGO_ENABLED=0 GOOS=linux go build -ldflags="-s -w" -o echo-server .

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /src/echo-server /echo-server
EXPOSE 8080
USER nonroot:nonroot
ENTRYPOINT ["/echo-server"]
