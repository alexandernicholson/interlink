FROM golang:1.23.4-alpine3.20 AS builder
WORKDIR /src
COPY echo-server.go go.mod ./
RUN CGO_ENABLED=0 GOOS=linux go build -ldflags="-s -w" -o echo-server .

FROM scratch
COPY --from=builder /src/echo-server /echo-server
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/echo-server"]
