# L4 forwarding benchmark

This directory contains an independent Linux-only benchmark for:

- Nginx stream;
- Caddy layer4 (`mholt/caddy-l4`);
- Raddex L4 (native Tokio data path);
- Linux NAT / nftables as a kernel-forwarding reference.

It does not share the HTTP benchmark's result directory or ranking. The
user-space proxies are compared with the same origin, payload, client image,
network topology, and resource limits. Linux NAT is reported as a separate
kernel reference because its forwarding work and connection state are not owned
by a user-space proxy process.

## Run it

The host must be Linux with Docker Engine, Docker Compose v2, and enough file
descriptors for the selected scale:

```bash
./bench/l4/scripts/preflight.sh
./bench/l4/scripts/run.sh quick
./bench/l4/scripts/run.sh full
./bench/l4/scripts/run.sh test
```

`quick` covers 10K connection and flow scenarios with short measurements. It
is useful for validating the topology and producing a first comparison.
`full` adds 50K/100K scenarios, payload-size variants, connection-size sweeps,
and three repetitions. It is intentionally not part of the normal Rust CI
path.
Each run uses a unique Compose project by default, so interrupted or concurrent
runs do not reuse one another's containers; set `L4_COMPOSE_PROJECT_NAME` only
when deliberate project reuse is required.

The target quota can be adjusted for a controlled local run:

```bash
L4_BENCH_CPUS=4 L4_BENCH_MEMORY_LIMIT=2g ./bench/l4/scripts/run.sh quick
```

The Caddy target is built from the pinned Caddy builder image and the pinned
`mholt/caddy-l4` release in `versions.env`. The plugin is an experimental
third-party Caddy app; its version and configuration are part of the benchmark
manifest.

## Topology

User-space targets use the same two-network path:

```text
loadgen -- client_net --> proxy -- origin_net --> origin
```

The NAT reference uses a privileged router container with nftables rules:

```text
loadgen -- client_net --> nftables router -- origin_net --> origin
```

Only TCP port `19000` and UDP port `19001` on the origin are reachable through
the router. DNAT maps client ports `18000` and `18001`; masquerade provides the
return path. The router applies the policy documented in
`configs/nftables/rules.nft.example` using the runtime interface addresses.

Each target is started alone. The origin, client image, target resource quota,
payload, and benchmark duration remain unchanged between targets. The two

## Fairness

All three user-space proxies get the same origin, payload, client image,
network topology, CPU quota, memory limit, and file-descriptor limit. Nginx runs
two workers and Raddex two threads; Caddy is bounded by the same CPU quota.

No target is given tuning the others cannot have. In particular the Raddex
config does **not** set `recv_buffer` / `send_buffer` on its UDP listener:
neither Nginx stream nor Caddy layer4 exposes an equivalent per-listener knob,
so all three run on the OS default socket buffers.

Linux NAT / nftables is reported as a *kernel reference*, not ranked against the
proxies — its forwarding work and connection state are not owned by a user-space
process, so its CPU and memory columns are intentionally blank.

## Scenarios

The matrix is defined in `scenarios/scenarios.json`:

| Kind | Coverage |
| --- | --- |
| TCP throughput | 64 KiB echo payload with 1, 16, and 64 connections |
| UDP throughput | 64 B, 512 B, and 1400 B datagrams at a configured offered PPS |
| UDP PPS | 64 B datagrams with a configured offered PPS |
| p99 latency | TCP and UDP 64 B request-response |
| TCP connection rate | 10K, 50K, and 100K connection establishment |
| TCP long-lived | 10K, 50K, and 100K established connections held open |
| UDP flow capacity | 10K, 50K, and 100K client flows held open |

TCP connection scenarios first read the origin's `READY` handshake. This
separates downstream accept success from successful upstream establishment.
UDP flow scenarios send and receive one probe per source socket, so the flow is
known to be active before the hold interval begins.

Every repetition runs its warmup pass, then **restarts the target** before the
measured pass. The warmup leaves the target's network namespace with one
`TIME-WAIT` socket toward the origin per warmup connection, plus the sockets
the load generator's exit orphaned. A 10K-connection warmup fills one parity
of the ephemeral port range, after which every `connect()` in a measured pass
on the same namespace first scans the occupied ports: 1–5 ms per connect
against 0.1 ms on a clean namespace, in proportion to how few threads the
target connects from. Measured before the restart was added, that scan alone
moved the 10K connection-rate result from 89% to 49% of Nginx for a target
connecting from one thread. The restart makes the connection-rate and
long-lived scenarios measure the proxy on identical kernel state rather than
the kernel's port scan.

The same port range also caps what one proxy address can hold open toward one
origin address: with the default `ip_local_port_range` (32768–60999) that is
28 232 concurrent proxied connections, before any `TIME-WAIT` residue. The 50K
and 100K connection scenarios ask for more than that from every target, so
their results describe where this topology saturates, not a ranking.

UDP throughput is paced instead of flooding the socket indefinitely. The
report records both offered PPS and received PPS plus packet loss. A result
with high packet loss is a stress point, not evidence of higher useful
throughput.

## Metrics and fairness

Every scenario is normalized independently against Nginx stream:

```text
Nginx = 1.00x = 100%
```

The overview contains separate panels for:

- TCP throughput;
- UDP throughput;
- UDP packets per second;
- established TCP connections;
- TCP connection establishment rate;
- long-lived TCP CPU cost for user-space targets;
- established UDP flows;
- p99 latency;
- peak user-space memory.

Throughput, PPS, connection rate, and established capacity are higher-is-better.
p99 latency, CPU cost, and memory are lower-is-better. Error rate and UDP
packet loss are absolute percentages and are never normalized.

User-space CPU is calculated from the target container cgroup. Kernel NAT
forwarding is not charged to the router process cgroup, so NAT CPU and RSS are
not placed in the user-space normalized CPU/memory panels. The raw result keeps
host total CPU, host softirq time, conntrack entries, and `nf_conntrack` slab
objects/bytes as separate fields. The byte value is an approximate active-slab
footprint. The monitor also records cgroup-v2 `memory.current`, `memory.peak`,
the `memory.stat` anon/file/kernel/socket fields, and current process/thread
counts when the host exposes them. These fields must be interpreted as raw
accounting signals, not as a replacement for the normalized peak-memory metric.

The run manifest records the Raddex commit, kernel release, tool versions,
scenario/configuration and benchmark-input hashes, plus CPU/memory quotas,
Raddex thread count, loadgen shard count, and optional perf availability. It does
not record a host name or merge absolute results from different machines.

## Scale prerequisites

The 100K scenarios need multiple source ports and high descriptor limits. The
preflight checks the most important host values, including:

- `ulimit -n`;
- `ip_local_port_range`;
- `somaxconn` and `tcp_max_syn_backlog`;
- `nf_conntrack_max`;
- availability of `perf` for optional NAT kernel counters.

The runner opens connections in controlled batches using multiple loadgen
containers, giving each shard a distinct Docker source address. A failed
connection or flow is retained in the result and causes the collector to report
its success rate instead of silently treating the requested count as
established.

## Results

Run data is written below `bench/l4/results/` and is ignored by Git:

```text
bench/l4/results/<run-id>/
  raw/             loadgen JSON, Docker stats, host CPU, and perf samples
  summary.json     median raw and normalized metrics
  summary.csv      flattened raw and normalized metrics
  report.md        Markdown report
  report.html      browsable report
  charts/overview.svg
  charts/overview.png
```

The tracked overview snapshot is `bench/l4/overview.svg`. The documentation
site receives a synchronized copy at
`page/public/benchmarks/l4-forwarding.svg`.

The benchmark is intentionally opt-in. Do not compare absolute throughput or
latency between machines, and do not treat the kernel NAT reference as a
drop-in replacement for proxy features such as TLS termination, SNI routing,
or application-layer policy.
