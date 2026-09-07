# syntax=docker/dockerfile:1.6
# Multi-arch (amd64/arm64) distroless image for OxiGate

FROM --platform=$BUILDPLATFORM rust:1.75-bookworm AS builder
ARG TARGETPLATFORM
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY examples ./examples
# Create dummy lock-friendly build
RUN mkdir -p src && cargo build --release 2>/dev/null || true
COPY . .
RUN cargo build --release && strip target/release/oxigate || true

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/oxigate /usr/local/bin/oxigate
COPY examples/config.full.yaml /etc/oxigate/config.yaml
USER nonroot:nonroot
EXPOSE 8080 9090
ENTRYPOINT ["/usr/local/bin/oxigate"]
CMD ["--config", "/etc/oxigate/config.yaml"]
