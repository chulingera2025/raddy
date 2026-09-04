# Benchmark TLS material

`bench.crt` and `bench.key` are self-signed, non-production credentials used
only to make the Nginx, Caddy, and Raddex HTTPS listeners identical. The key is
not an application secret and must never be reused outside this benchmark.
