ARG RUST_IMAGE
ARG RUNTIME_IMAGE
FROM ${RUST_IMAGE} AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM ${RUNTIME_IMAGE}

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates=20250419~deb12u1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/l4-bench-loadgen /usr/local/bin/l4-bench-loadgen
ENTRYPOINT ["/usr/local/bin/l4-bench-loadgen"]
