#!/usr/bin/env bash
# Run the containerized Nginx stream/Caddy layer4/Raddex L4/NAT benchmark.
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
ROOT_DIR="$(cd -- "$BENCH_DIR/../.." && pwd)"
MODE="${1:-quick}"

case "$MODE" in
    quick|full|test|preflight) ;;
    *)
        printf 'usage: %s {quick|full|test|preflight}\n' "$0" >&2
        exit 2
        ;;
esac

if [[ "$MODE" == "preflight" ]]; then
    exec "$SCRIPT_DIR/preflight.sh"
fi

export L4_BENCH_CPUS="${L4_BENCH_CPUS:-2.0}"
export L4_BENCH_MEMORY_LIMIT="${L4_BENCH_MEMORY_LIMIT:-1g}"
export RADDEX_THREADS="${RADDEX_THREADS:-2}"
export L4_LOADGEN_SHARDS="${L4_LOADGEN_SHARDS:-4}"
# A unique default project keeps an interrupted or concurrent run from
# reusing another run's containers and networks. Override it only deliberately.
PROJECT_NAME="${L4_COMPOSE_PROJECT_NAME:-raddex-l4-bench-$$}"
COMPOSE=(docker compose --env-file "$BENCH_DIR/versions.env" -f "$BENCH_DIR/compose.yaml" -p "$PROJECT_NAME")

die() {
    printf 'L4 benchmark error: %s\n' "$*" >&2
    exit 1
}


cleanup_monitor() {
    if [[ -n "${MONITOR_PID:-}" ]]; then
        kill "$MONITOR_PID" 2>/dev/null || true
        wait "$MONITOR_PID" 2>/dev/null || true
        MONITOR_PID=""
    fi
}

cleanup() {
    cleanup_monitor
    "${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
}

trap cleanup EXIT INT TERM

if ! command -v docker >/dev/null 2>&1; then
    die "Docker is required"
fi
docker compose version >/dev/null 2>&1 || die "Docker Compose v2 is required"
[[ "$L4_LOADGEN_SHARDS" =~ ^[1-9][0-9]*$ ]] || die "L4_LOADGEN_SHARDS must be a positive integer"
"$SCRIPT_DIR/preflight.sh"
PERF_SYSTEM_WIDE=0
if [[ "${L4_ENABLE_PERF:-1}" == "1" && -x "$(command -v perf 2>/dev/null || true)" ]] \
    && perf stat -a -e task-clock -o /dev/null -- true >/dev/null 2>&1; then
    PERF_SYSTEM_WIDE=1
fi

if [[ "$MODE" == "test" ]]; then
    "${COMPOSE[@]}" build --pull reporter
    "${COMPOSE[@]}" run --rm --no-deps reporter /bench-l4/scripts/test_reports.py
    exit 0
fi

RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
RUN_DIR="$BENCH_DIR/results/$RUN_ID"
RAW_DIR="$RUN_DIR/raw"
mkdir -p "$RAW_DIR"

RADDEX_COMMIT="$(git -C "$ROOT_DIR" rev-parse HEAD 2>/dev/null || printf 'unknown')"
if [[ -n "$(git -C "$ROOT_DIR" status --porcelain --untracked-files=all 2>/dev/null)" ]]; then
    RADDEX_COMMIT="${RADDEX_COMMIT}-dirty"
fi

printf 'Building pinned L4 benchmark images...\n'
"${COMPOSE[@]}" build --pull origin loadgen reporter raddex caddy router
"${COMPOSE[@]}" pull nginx

printf 'Starting the shared L4 origin...\n'
"${COMPOSE[@]}" up -d --wait origin
"${COMPOSE[@]}" run --rm --no-deps reporter /bench-l4/scripts/collect.py \
    --write-manifest \
    --bench-root /bench-l4 \
    --run-id "$RUN_ID" \
    --profile "$MODE" \
    --raddex-commit "$RADDEX_COMMIT" \
    --cpu-limit "$L4_BENCH_CPUS" \
    --memory-limit "$L4_BENCH_MEMORY_LIMIT" \
    --raddex-threads "$RADDEX_THREADS" \
    --loadgen-shards "$L4_LOADGEN_SHARDS" \
    --perf-system-wide "$PERF_SYSTEM_WIDE"

PLAN_FILE="$RUN_DIR/plan.txt"
"${COMPOSE[@]}" run --rm --no-deps reporter /bench-l4/scripts/collect.py \
    --plan-shell \
    --scenario-file /bench-l4/scenarios/scenarios.json \
    --profile "$MODE" >"$PLAN_FILE"

target_service() {
    local target="$1"
    case "$target" in
        nat) printf '%s' router ;;
        raddex-l4) printf '%s' raddex ;;
        *) printf '%s' "$target" ;;
    esac
}

host_snapshot() {
    awk '
        /^cpu / { total = 0; for (i = 2; i <= 11; i++) total += $i; idle = $5 + $6 }
        /^softirq / { softirq = 0; for (i = 2; i <= NF; i++) softirq += $i }
        END { printf "%.0f\t%.0f\t%.0f\t%.0f\n", systime(), total, idle, softirq }
    ' /proc/stat
}

start_monitor() {
    local container_id="$1"
    local target="$2"
    local stats_file="$3"
    local container_pid cgroup_path cgroup_dir
    container_pid="$(docker inspect --format '{{.State.Pid}}' "$container_id" 2>/dev/null || true)"
    printf 'timestamp\tcpu_percent\tmemory_usage\tconntrack_count\tnf_conntrack_objects\tnf_conntrack_bytes\tmemory_current\tmemory_peak_cgroup\tmemory_anon\tmemory_file\tmemory_kernel\tmemory_sock\tpids_current\tthreads_current\n' >"$stats_file"
    (
        while true; do
            line="$(docker stats --no-stream --format '{{.CPUPerc}}\t{{.MemUsage}}' "$container_id" 2>/dev/null || true)"
            if [[ -n "$line" ]]; then
                conntrack=""
                nf_objects=""
                nf_bytes=""
                if [[ "$target" == "nat" ]]; then
                    conntrack="$(docker exec "$container_id" conntrack -C 2>/dev/null || true)"
                    nf_objects="$(docker exec "$container_id" awk '$1 == "nf_conntrack" { print $2; exit }' /proc/slabinfo 2>/dev/null || true)"
                    nf_bytes="$(docker exec "$container_id" awk '$1 == "nf_conntrack" { print $2 * $4; exit }' /proc/slabinfo 2>/dev/null || true)"
                fi
                cgroup_current=""
                cgroup_peak=""
                cgroup_anon=""
                cgroup_file=""
                cgroup_kernel=""
                cgroup_sock=""
                cgroup_pids=""
                cgroup_threads=""
                if [[ "$container_pid" =~ ^[0-9]+$ && -r "/proc/$container_pid/cgroup" ]]; then
                    cgroup_path="$(awk -F: '$1 == "0" { print $3; exit }' "/proc/$container_pid/cgroup")"
                    cgroup_dir="/sys/fs/cgroup$cgroup_path"
                    if [[ -r "$cgroup_dir/memory.current" ]]; then
                        cgroup_current="$(cat "$cgroup_dir/memory.current")"
                        cgroup_peak="$(cat "$cgroup_dir/memory.peak")"
                        cgroup_anon="$(awk '$1 == "anon" { print $2; exit }' "$cgroup_dir/memory.stat")"
                        cgroup_file="$(awk '$1 == "file" { print $2; exit }' "$cgroup_dir/memory.stat")"
                        cgroup_kernel="$(awk '$1 == "kernel" { print $2; exit }' "$cgroup_dir/memory.stat")"
                        cgroup_sock="$(awk '$1 == "sock" { print $2; exit }' "$cgroup_dir/memory.stat")"
                        cgroup_pids="$(cat "$cgroup_dir/pids.current" 2>/dev/null || true)"
                        cgroup_threads="$(awk '/^Threads:/ { print $2; exit }' "/proc/$container_pid/status" 2>/dev/null || true)"
                    fi
                fi
                printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
                    "$(date +%s.%N)" "$line" "$conntrack" "$nf_objects" "$nf_bytes" \
                    "$cgroup_current" "$cgroup_peak" "$cgroup_anon" "$cgroup_file" \
                    "$cgroup_kernel" "$cgroup_sock" "$cgroup_pids" "$cgroup_threads"
            fi
            sleep "${L4_STATS_INTERVAL:-0.25}"
        done
    ) >>"$stats_file" &
    MONITOR_PID=$!
}

run_loadgen() {
    local target="$1"
    local mode="$2"
    local port="$3"
    local duration="$4"
    local connections="$5"
    local payload_bytes="$6"
    local window="$7"
    local connect_rate="$8"
    local packets_per_second="$9"
    local connect_timeout="${10}"
    local result_path="${11}"
    local stderr_path="${12}"
    local perf_path="${13}"
    local service
    service="$(target_service "$target")"
    local args=(
        --mode "$mode"
        --host "$service"
        --port "$port"
        --duration-secs "$duration"
        --connections "$connections"
        --payload-bytes "$payload_bytes"
        --window "$window"
        --packets-per-second "$packets_per_second"
        --connect-timeout-ms "$connect_timeout"
    )
    if (( connect_rate > 0 )); then
        args+=(--connect-rate "$connect_rate")
    fi

    if [[ "$mode" == "tcp-connections" || "$mode" == "tcp-connect-rate" || "$mode" == "udp-flows" ]] \
        && (( connections >= 10000 )); then
        run_sharded_loadgen "$target" "$mode" "$port" "$duration" "$connections" "$payload_bytes" "$window" "$connect_rate" "$connect_timeout" "$result_path" "$stderr_path" "$perf_path"
        return $?
    fi

    local command=("${COMPOSE[@]}" run --rm --no-deps loadgen "${args[@]}")
    : >"$perf_path"
    if [[ "$target" == "nat" && "$PERF_SYSTEM_WIDE" == "1" ]]; then
        perf stat -a -x, -e task-clock,context-switches,cpu-migrations,page-faults \
            -o "$perf_path" -- "${command[@]}" </dev/null >"$result_path" 2>"$stderr_path"
    else
        "${command[@]}" </dev/null >"$result_path" 2>"$stderr_path"
    fi
}

probe_target() {
    local target="$1"
    local transport="$2"
    local port="$3"
    local connect_timeout="$4"
    local service
    service="$(target_service "$target")"
    local mode=tcp-latency
    if [[ "$transport" == "udp" ]]; then
        mode=udp-latency
    fi
    local output
    for _ in {1..60}; do
        if output=$("${COMPOSE[@]}" run --rm --no-deps loadgen \
            --mode "$mode" \
            --host "$service" \
            --port "$port" \
            --duration-secs 1 \
            --connections 1 \
            --payload-bytes 1 \
            --window 1 \
            --connect-timeout-ms "$connect_timeout" </dev/null 2>/dev/null) \
            && printf '%s' "$output" | grep -Eq '"completed_operations"[[:space:]]*:[[:space:]]*[1-9]'; then
            return 0
        fi
        sleep 1
    done
    return 1
}

start_target() {
    local target="$1"
    local transport="$2"
    local port="$3"
    local connect_timeout="$4"
    local service
    service="$(target_service "$target")"
    printf 'Starting %s (%s)...\n' "$target" "$transport"
    "${COMPOSE[@]}" up -d "$service"
    local container_id
    container_id="$("${COMPOSE[@]}" ps -q "$service")"
    [[ -n "$container_id" ]] || die "could not resolve the container id for $target"
    probe_target "$target" "$transport" "$port" "$connect_timeout" || die "$target did not pass the L4 probe"
}

stop_target() {
    local target="$1"
    "${COMPOSE[@]}" stop "$(target_service "$target")" >/dev/null
}

run_sharded_loadgen() {
    local target="$1"
    local mode="$2"
    local port="$3"
    local duration="$4"
    local connections="$5"
    local payload_bytes="$6"
    local window="$7"
    local connect_rate="$8"
    local connect_timeout="$9"
    local result_path="${10}"
    local stderr_path="${11}"
    local perf_path="${12}"
    local service
    service="$(target_service "$target")"
    local shard_dir="${result_path%.json}.shards"
    mkdir -p "$shard_dir"
    : >"$perf_path"
    local shard_connections=$(( (connections + L4_LOADGEN_SHARDS - 1) / L4_LOADGEN_SHARDS ))
    local shard_rate=0
    if (( connect_rate > 0 )); then
        shard_rate=$(( (connect_rate + L4_LOADGEN_SHARDS - 1) / L4_LOADGEN_SHARDS ))
    fi
    local pids=()
    local shard_results=()
    for shard in $(seq 1 "$L4_LOADGEN_SHARDS"); do
        local completed=$(( (shard - 1) * shard_connections ))
        local shard_count=$shard_connections
        if (( completed + shard_count > connections )); then
            shard_count=$(( connections - completed ))
        fi
        (( shard_count > 0 )) || continue
        local shard_result="$shard_dir/shard-${shard}.json"
        local shard_error="$shard_dir/shard-${shard}.stderr.log"
        local shard_command=(
            "${COMPOSE[@]}" run --rm --no-deps loadgen
            --mode "$mode"
            --host "$service"
            --port "$port"
            --duration-secs "$duration"
            --connections "$shard_count"
            --payload-bytes "$payload_bytes"
            --window "$window"
            --packets-per-second 0
            --connect-timeout-ms "$connect_timeout"
        )
        if (( shard_rate > 0 )); then
            shard_command+=(--connect-rate "$shard_rate")
        fi
        "${shard_command[@]}" </dev/null >"$shard_result" 2>"$shard_error" &
        pids+=("$!")
        shard_results+=("$shard_result")
    done
    local shard_failed=0
    for pid in "${pids[@]}"; do
        if ! wait "$pid"; then
            shard_failed=1
        fi
    done
    if (( shard_failed != 0 )); then
        printf 'one or more loadgen shards failed; see %s\n' "$shard_dir" >"$stderr_path"
        return 1
    fi

    local output_in_container="/bench-l4${result_path#$BENCH_DIR}"
    local inputs_in_container=()
    for shard_result in "${shard_results[@]}"; do
        inputs_in_container+=("/bench-l4${shard_result#$BENCH_DIR}")
    done
    "${COMPOSE[@]}" run --rm --no-deps reporter /bench-l4/scripts/merge_loadgen.py \
        --output "$output_in_container" "${inputs_in_container[@]}" </dev/null > /dev/null 2>"$stderr_path"
}

run_scenario_target() {
    local scenario_id="$1"
    local title="$2"
    local kind="$3"
    local mode="$4"
    local transport="$5"
    local port="$6"
    local connections="$7"
    local payload_bytes="$8"
    local window="$9"
    local connect_rate="${10}"
    local packets_per_second="${11}"
    local warmup="${12}"
    local duration="${13}"
    local repetitions="${14}"
    local connect_timeout="${15}"
    local target

    for target in nginx caddy raddex-l4 nat; do
        start_target "$target" "$transport" "$port" "$connect_timeout"
        local container_id
        container_id="$("${COMPOSE[@]}" ps -q "$(target_service "$target")")"
        for repetition in $(seq 1 "$repetitions"); do
            local point_dir="$RAW_DIR/$scenario_id/$target"
            mkdir -p "$point_dir"
            local result_path="$point_dir/rep-${repetition}.json"
            local stderr_path="$point_dir/rep-${repetition}.stderr.log"
            local stats_path="$point_dir/rep-${repetition}.stats.tsv"
            local host_path="$point_dir/rep-${repetition}.host.tsv"
            local perf_path="$point_dir/rep-${repetition}.perf.csv"
            local meta_path="$point_dir/rep-${repetition}.meta.json"

            if (( warmup > 0 )); then
                local warmup_result="$point_dir/rep-${repetition}.warmup.json"
                local warmup_error="$point_dir/rep-${repetition}.warmup.stderr.log"
                local warmup_perf="$point_dir/rep-${repetition}.warmup.perf.csv"
                run_loadgen "$target" "$mode" "$port" "$warmup" "$connections" "$payload_bytes" "$window" "$connect_rate" "$packets_per_second" \
                    "$connect_timeout" "$warmup_result" "$warmup_error" "$warmup_perf" || die "warmup failed for $scenario_id / $target"
                # The warmup leaves the target's netns with every warmup
                # connection in TIME-WAIT toward the origin plus whatever the
                # loadgen's exit orphaned. A 10K-connection warmup fills one
                # parity of the ephemeral port range, and from then on each
                # connect() in the measured pass scans the occupied ports
                # first: measured at 1-5 ms per connect against 0.1 ms clean,
                # scaling with how many threads the target connects from. The
                # measured pass therefore starts from a fresh container so it
                # measures the proxy, not the kernel's port scan.
                stop_target "$target"
                start_target "$target" "$transport" "$port" "$connect_timeout"
                container_id="$("${COMPOSE[@]}" ps -q "$(target_service "$target")")"
            fi

            host_snapshot >"$host_path"
            start_monitor "$container_id" "$target" "$stats_path"
            set +e
            run_loadgen "$target" "$mode" "$port" "$duration" "$connections" "$payload_bytes" "$window" "$connect_rate" "$packets_per_second" \
                "$connect_timeout" "$result_path" "$stderr_path" "$perf_path"
            exit_code=$?
            set -e
            cleanup_monitor
            host_snapshot >>"$host_path"
            printf '{"scenario_id":"%s","title":"%s","kind":"%s","target":"%s","transport":"%s","connections":%s,"payload_bytes":%s,"repetition":%s,"exit_code":%s,"connect_timeout_ms":%s}\n' \
                "$scenario_id" "$title" "$kind" "$target" "$transport" "$connections" "$payload_bytes" "$repetition" "$exit_code" "$connect_timeout" >"$meta_path"
            (( exit_code == 0 )) || die "load generator failed for $scenario_id / $target / rep-${repetition}"
        done
        stop_target "$target"
    done
}

while IFS='|' read -r scenario_id title kind mode transport port connections payload_bytes window connect_rate packets_per_second warmup duration repetitions connect_timeout; do
    [[ -z "$scenario_id" ]] && continue
    printf 'Running %s (%s)...\n' "$scenario_id" "$kind"
    run_scenario_target "$scenario_id" "$title" "$kind" "$mode" "$transport" "$port" "$connections" "$payload_bytes" "$window" "$connect_rate" "$packets_per_second" "$warmup" "$duration" "$repetitions" "$connect_timeout"
done <"$PLAN_FILE"

"${COMPOSE[@]}" run --rm --no-deps reporter /bench-l4/scripts/collect.py \
    --collect \
    --bench-root /bench-l4 \
    --raw-dir "/bench-l4/results/$RUN_ID/raw" \
    --scenario-file /bench-l4/scenarios/scenarios.json \
    --profile "$MODE" \
    --manifest "/bench-l4/results/$RUN_ID/run.json" \
    --summary-out "/bench-l4/results/$RUN_ID/summary.json"

"${COMPOSE[@]}" run --rm --no-deps reporter /bench-l4/scripts/report.py \
    --summary "/bench-l4/results/$RUN_ID/summary.json" \
    --output-dir "/bench-l4/results/$RUN_ID" \
    --public-dir /public/benchmarks \
    --stable-bench-dir /bench-l4

printf '\nL4 benchmark complete.\nRun directory: %s\nMarkdown report: %s/report.md\nHTML report: %s/report.html\nStable overview: %s/overview.svg\n' \
    "$RUN_DIR" "$RUN_DIR" "$RUN_DIR" "$BENCH_DIR"
