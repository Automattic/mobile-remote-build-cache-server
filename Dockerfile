# syntax=docker/dockerfile:1

# Build a static musl binary for the platform being built. For both production
# architectures:
#   docker buildx build --platform linux/amd64,linux/arm64 -t apps-infra-mobile-build-cache-server .
# The Rust version is pinned here and in rust-toolchain.toml; keep them in step.
FROM rust:1.99.1-alpine AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked \
    && cp target/release/apps-infra-mobile-build-cache-server /server \
    && mkdir /data

# Nothing but the binary: no shell, no libc, no package manager.
FROM scratch
COPY --from=build /server /apps-infra-mobile-build-cache-server
# An empty, correctly owned mount point for the data volume.
COPY --from=build --chown=5000:5000 /data /data
# Numeric, unprivileged; 5000:5000 owns the existing production volume.
USER 5000:5000
VOLUME /data
EXPOSE 5071 9090
# There is no shell or curl in the image, so the binary checks itself.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/apps-infra-mobile-build-cache-server", "healthcheck"]
ENTRYPOINT ["/apps-infra-mobile-build-cache-server"]
