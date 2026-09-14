# syntax=docker/dockerfile:1
ARG GO_VERSION=1.27.1
FROM --platform=$BUILDPLATFORM golang:${GO_VERSION}-alpine AS build
ARG TARGETOS=linux
ARG TARGETARCH=amd64
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY *.go ./
COPY static ./static
RUN CGO_ENABLED=0 GOOS=${TARGETOS} GOARCH=${TARGETARCH} go build -trimpath -ldflags="-s -w" -o /out/flow-panel .

FROM alpine:3
COPY --from=build /out/flow-panel /usr/local/bin/flow-panel
COPY THIRD_PARTY_NOTICES.txt /usr/share/doc/flow-panel/THIRD_PARTY_NOTICES.txt
WORKDIR /abc
VOLUME ["/abc"]
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/flow-panel"]
