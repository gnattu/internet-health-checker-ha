# Home Assistant add-on Dockerfile (multi-stage build for Rust service)

ARG BUILD_FROM
ARG BUILDPLATFORM
ARG TARGETPLATFORM
ARG TARGETOS
ARG TARGETARCH
ARG TARGETVARIANT

# Build stage
FROM --platform=$BUILDPLATFORM rust:1.80-bullseye as build
WORKDIR /app

# Cache dependencies
COPY service/Cargo.toml ./service/Cargo.toml
RUN mkdir -p service/src && echo "fn main(){}" > service/src/main.rs && \
    cd service && cargo build --release && rm -rf target/release/deps/internet_health_checker*

# Build actual source
COPY service ./service
RUN cd service && cargo build --release

# Runtime stage based on Home Assistant base image
FROM ${BUILD_FROM}
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
RUN if command -v apt-get >/dev/null 2>&1; then \
      apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
      rm -rf /var/lib/apt/lists/*; \
    else \
      echo "Non-Debian base; assuming certs present"; \
    fi

# Copy rootfs (s6 service scripts)
COPY internet_health_checker/rootfs /

# Copy compiled binary
COPY --from=build /app/service/target/release/internet-health-checker /usr/bin/internet-health-checker
RUN chmod +x /usr/bin/internet-health-checker && chmod +x /etc/services.d/app/run

# Expose port used by the HTTP server
EXPOSE 8000
