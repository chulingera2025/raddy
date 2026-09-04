#!/usr/bin/env bash
# Check host prerequisites for the privileged Linux L4 benchmark.
set -Eeuo pipefail

die() {
    printf 'L4 benchmark preflight error: %s\n' "$*" >&2
    exit 1
}

[[ "$(uname -s)" == "Linux" ]] || die "the L4 benchmark requires Linux"
command -v docker >/dev/null 2>&1 || die "Docker is required"
docker compose version >/dev/null 2>&1 || die "Docker Compose v2 is required"

open_files="$(ulimit -n)"
if [[ "$open_files" != "unlimited" && "$open_files" -lt 100000 ]]; then
    die "ulimit -n must be at least 100000 for the 50K scenarios (current: $open_files)"
fi
loadgen_shards="${L4_LOADGEN_SHARDS:-4}"
[[ "$loadgen_shards" =~ ^[1-9][0-9]*$ ]] || die "L4_LOADGEN_SHARDS must be a positive integer"

printf 'Linux L4 benchmark prerequisites\n'
printf '  Docker Compose: available\n'
printf '  open files: %s\n' "$open_files"
printf '  ip_local_port_range: %s\n' "$(sysctl -n net.ipv4.ip_local_port_range 2>/dev/null || printf 'unavailable')"
printf '  somaxconn: %s\n' "$(sysctl -n net.core.somaxconn 2>/dev/null || printf 'unavailable')"
printf '  tcp_max_syn_backlog: %s\n' "$(sysctl -n net.ipv4.tcp_max_syn_backlog 2>/dev/null || printf 'unavailable')"
conntrack_max="$(sysctl -n net.netfilter.nf_conntrack_max 2>/dev/null || printf 'unavailable')"
printf '  nf_conntrack_max: %s\n' "$conntrack_max"
[[ "$conntrack_max" =~ ^[0-9]+$ ]] || die "could not read nf_conntrack_max"
if (( conntrack_max < 50000 )); then
    die "nf_conntrack_max must be at least 50000 for the 50K scenarios (current: $conntrack_max)"
fi

port_range="$(sysctl -n net.ipv4.ip_local_port_range 2>/dev/null || true)"
[[ "$port_range" =~ ^([0-9]+)[[:space:]]+([0-9]+)$ ]] || die "could not read ip_local_port_range"
port_count=$((BASH_REMATCH[2] - BASH_REMATCH[1] + 1))
required_ports=$(( (50000 + loadgen_shards - 1) / loadgen_shards ))
if (( port_count < required_ports )); then
    die "ip_local_port_range provides $port_count ports per shard; at least $required_ports are required for 50K"
fi
if command -v perf >/dev/null 2>&1 && perf stat -a -e task-clock -o /dev/null -- true >/dev/null 2>&1; then
    printf '  perf: available (NAT kernel CPU samples enabled)\n'
else
    printf '  perf: unavailable for system-wide sampling (NAT kernel CPU samples will be incomplete)\n'
fi
