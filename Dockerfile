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
RUN cargo build --release --locked -p fastmock-server --bin fastmock

FROM ${RUNTIME_IMAGE}
COPY --from=build /src/target/release/fastmock /usr/local/bin/fastmock
LABEL org.opencontainers.image.title="fastmock" \
      org.opencontainers.image.licenses="Apache-2.0"
USER 65532:65532
EXPOSE 8080 9090
ENTRYPOINT ["/usr/local/bin/fastmock"]
CMD ["serve"]
