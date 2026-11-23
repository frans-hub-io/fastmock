ARG NODE_IMAGE=docker.io/library/node:22.23.5-alpine3.24
ARG RUST_IMAGE=docker.io/library/rust:1.100.0-slim-trixie
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot

FROM --platform=$BUILDPLATFORM ${NODE_IMAGE} AS ui
ENV SCARF_ANALYTICS=false \
    npm_config_audit=false \
    npm_config_fund=false \
    npm_config_update_notifier=false
WORKDIR /src/ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY ui/ ./
RUN npm run build

FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY --from=ui /src/ui/dist ./ui/dist
ENV CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_DEBUG=0 \
    CARGO_TERM_COLOR=never \
    RUSTFLAGS="--remap-path-prefix=/usr/local/cargo=/cargo --remap-path-prefix=/usr/local/rustup=/rustup --remap-path-prefix=/src=/build"
RUN cargo build --release --locked -p fastmock-server --bin fastmock -p fastmock-operator --bin fastmock-operator \
 && install -D -m 0755 target/release/fastmock /out/server/usr/local/bin/fastmock \
 && install -D -m 0755 target/release/fastmock-operator /out/operator/usr/local/bin/fastmock-operator

FROM ${RUNTIME_IMAGE} AS operator
ARG VERSION=0.1.0-dev15
ARG REVISION=unknown
LABEL org.opencontainers.image.title="FastMock operator" \
      org.opencontainers.image.description="Kubernetes operator for FastMock servers and mock APIs" \
      org.opencontainers.image.url="https://fastmock.franshub.io/" \
      org.opencontainers.image.documentation="https://fastmock.franshub.io/" \
      org.opencontainers.image.source="https://github.com/frans-hub-io/fastmock" \
      org.opencontainers.image.vendor="FransHub" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
COPY --from=build /out/operator/usr/local/bin/fastmock-operator /usr/local/bin/fastmock-operator
USER 65532:65532
EXPOSE 8081
ENTRYPOINT ["/usr/local/bin/fastmock-operator"]

FROM ${RUNTIME_IMAGE} AS server
ARG VERSION=0.1.0-dev15
ARG REVISION=unknown
LABEL org.opencontainers.image.title="FastMock" \
      org.opencontainers.image.description="API mock server with durable callbacks, shared state, message brokers and full observability" \
      org.opencontainers.image.url="https://fastmock.franshub.io/" \
      org.opencontainers.image.documentation="https://fastmock.franshub.io/" \
      org.opencontainers.image.source="https://github.com/frans-hub-io/fastmock" \
      org.opencontainers.image.vendor="FransHub" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
COPY --from=build /out/server/usr/local/bin/fastmock /usr/local/bin/fastmock
USER 65532:65532
EXPOSE 8080 9090
ENTRYPOINT ["/usr/local/bin/fastmock"]
CMD ["serve"]
