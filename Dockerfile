# syntax=docker/dockerfile:1
# Override these with registry mirrors / digests in a controlled release build.
ARG RUST_IMAGE=rust:1.98.0-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim
FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations
ARG CARGO_BUILD_JOBS=2
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --locked --release -j ${CARGO_BUILD_JOBS} -p hibana-control-plane -p hibana-worker \
    && mkdir /out \
    && cp target/release/hibana-control-plane target/release/hibana-worker /out/

FROM ${RUNTIME_IMAGE} AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/cache/hibana && chown 10001:10001 /var/cache/hibana
COPY --from=build /out/ /usr/local/bin/
ENV WASM_CACHE_DIR=/var/cache/hibana TMPDIR=/tmp LOG_FORMAT=json
USER 10001:10001
EXPOSE 8080 8081 9090
ENTRYPOINT ["/usr/local/bin/hibana-control-plane"]
