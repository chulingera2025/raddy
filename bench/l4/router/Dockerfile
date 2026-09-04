ARG RUNTIME_IMAGE
FROM ${RUNTIME_IMAGE}

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        conntrack=1:1.4.7-1+b2 \
        iproute2=6.1.0-3 \
        nftables=1.0.6-2+deb12u2 \
        procps=2:4.0.2-3 \
    && rm -rf /var/lib/apt/lists/*

COPY router.sh /usr/local/bin/l4-nft-router
RUN chmod 0755 /usr/local/bin/l4-nft-router
ENTRYPOINT ["/usr/local/bin/l4-nft-router"]
