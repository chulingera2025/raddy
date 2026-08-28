ARG RUST_IMAGE=docker.io/library/rust:1.97.1-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97
ARG RUNTIME_IMAGE=docker.io/library/debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
FROM ${RUST_IMAGE} AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM ${RUNTIME_IMAGE}

COPY --from=builder /src/target/release/bench-origin /usr/local/bin/bench-origin
EXPOSE 18080
ENTRYPOINT ["/usr/local/bin/bench-origin"]
