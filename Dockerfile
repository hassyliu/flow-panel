# syntax=docker/dockerfile:1

# ---- build: static musl binary ----
FROM rust:1-alpine AS build
RUN apk add --no-cache gcc musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=flow-panel-target \
    cargo build --release --locked \
 && mkdir -p /out/data \
 && cp target/release/flow-panel /out/flow-panel

# ---- runtime: just the binary (CA roots are built in) ----
FROM scratch
COPY --from=build /out/flow-panel /flow-panel
COPY --from=build --chown=65534:65534 /out/data /data
USER 65534:65534
ENV FLOW_PANEL_DATA_DIR=/data \
    FLOW_PANEL_PORT=8089
VOLUME ["/data"]
EXPOSE 8089
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/flow-panel", "--healthcheck"]
ENTRYPOINT ["/flow-panel"]
