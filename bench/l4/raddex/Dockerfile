ARG RUST_IMAGE
ARG RUNTIME_IMAGE
FROM ${RUST_IMAGE} AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libssl-dev=3.0.20-1~deb12u2 \
        pkg-config=1.8.1-1 \
        cmake=3.25.1-1 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM ${RUNTIME_IMAGE}

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates=20250419~deb12u1 libssl3=3.0.20-1~deb12u2 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/raddex /usr/local/bin/raddex
ENTRYPOINT ["/usr/local/bin/raddex"]
