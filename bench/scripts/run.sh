#!/usr/bin/env bash
# Run the containerized Nginx/Caddy/Raddex benchmark matrix.
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
ROOT_DIR="$(cd -- "$BENCH_DIR/.." && pwd)"
MODE="${1:-quick}"

case "$MODE" in
    quick|full|test) ;;
    *)
        printf 'usage: %s {quick|full|test}\n' "$0" >&2
        exit 2
        ;;
esac

export BENCH_CPUS="${BENCH_CPUS:-2.0}"
export BENCH_MEMORY_LIMIT="${BENCH_MEMORY_LIMIT:-1g}"
# Match the two Nginx workers under the default two-CPU benchmark quota. The
# production CLI keeps its conservative one-worker default independently.
export RADDEX_THREADS="${RADDEX_THREADS:-2}"
PROJECT_NAME="${COMPOSE_PROJECT_NAME:-raddex-bench}"
COMPOSE=(docker compose --env-file "$BENCH_DIR/versions.env" -f "$BENCH_DIR/compose.yaml" -p "$PROJECT_NAME")

die() {
    printf 'benchmark error: %s\n' "$*" >&2
    exit 1
}

if ! command -v docker >/dev/null 2>&1; then
    die "Docker is required"
fi
docker compose version >/dev/null 2>&1 || die "Docker Compose is required"

ensure_tls_material() {
    mkdir -p "$BENCH_DIR/tls"
    if [[ -s "$BENCH_DIR/tls/bench.crt" && -s "$BENCH_DIR/tls/bench.key" ]]; then
        return
    fi
    printf 'Generating the local benchmark TLS certificate...\n'
    "${COMPOSE[@]}" run --rm --no-deps --entrypoint openssl reporter req \
        -x509 -newkey rsa:2048 -nodes -days 3650 \
        -subj /CN=bench.local \
        -addext subjectAltName=DNS:bench.local \
        -keyout /bench/tls/bench.key \
        -out /bench/tls/bench.crt >/dev/null 2>&1
    chmod 0644 "$BENCH_DIR/tls/bench.crt"
    chmod 0600 "$BENCH_DIR/tls/bench.key"
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

if [[ "$MODE" == "test" ]]; then
    "${COMPOSE[@]}" build --pull reporter
    "${COMPOSE[@]}" run --rm --no-deps reporter /bench/scripts/test_reports.py
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

printf 'Building pinned benchmark images...\n'
"${COMPOSE[@]}" build --pull origin loadgen reporter raddex
"${COMPOSE[@]}" pull nginx caddy
ensure_tls_material

printf 'Starting the shared origin...\n'
"${COMPOSE[@]}" up -d --wait origin
"${COMPOSE[@]}" run --rm --no-deps reporter /bench/scripts/collect.py \
    --write-manifest \
    --bench-root /bench \
    --run-id "$RUN_ID" \
    --profile "$MODE" \
    --raddex-commit "$RADDEX_COMMIT" \
    --raddex-threads "$RADDEX_THREADS"

PLAN_FILE="$RUN_DIR/plan.txt"
"${COMPOSE[@]}" run --rm --no-deps reporter /bench/scripts/collect.py \
    --plan-shell \
    --scenario-file /bench/scenarios/scenarios.json \
    --profile "$MODE" >"$PLAN_FILE"

target_url() {
    local target="$1"
    local scheme="$2"
    local path="$3"
    local port=8080
    if [[ "$scheme" == "https" ]]; then
        port=8443
    fi
    printf '%s://bench.local:%s%s' "$scheme" "$port" "$path"
}

probe_target() {
    local target="$1"
    local scheme="$2"
    local protocol="$3"
    local url
    url="$(target_url "$target" "$scheme" "/healthz")"
    local port=8080
    if [[ "$scheme" == "https" ]]; then
        port=8443
    fi
    local args=(
        --no-tui
        --no-color
        --stats-success-breakdown
        --output-format json
        -n 1
        -c 1
        -t 5s
        --connect-to "bench.local:${port}:${target}:${port}"
        --host bench.local
    )
    if [[ "$scheme" == "https" ]]; then
        args+=(--insecure)
    fi
    if [[ "$protocol" == "http2" ]]; then
        args+=(--http2)
    fi
    args+=("$url")

    local output
    local body
    local expected_body
    expected_body="$(printf 's%.0s' {1..128})"
    for _ in {1..60}; do
        if output=$("${COMPOSE[@]}" run --rm --no-deps loadgen "${args[@]}" 2>/dev/null); then
            if printf '%s' "$output" | grep -Eq '"success_rate"[[:space:]]*:[[:space:]]*1([.]0+)?([,}]|$)'; then
                local curl_args=(
                    --fail
                    --silent
                    --show-error
                    --max-time 5
                    --connect-to "bench.local:${port}:${target}:${port}"
                    -H 'Host: bench.local'
                    "$(target_url "$target" "$scheme" "/small")"
                )
                if [[ "$scheme" == "https" ]]; then
                    curl_args+=(--insecure)
                fi
                if body=$("${COMPOSE[@]}" run --rm --no-deps --entrypoint curl loadgen "${curl_args[@]}" 2>/dev/null) \
                    && [[ "$body" == "$expected_body" ]]; then
                    return 0
                fi
            fi
        fi
        sleep 1
    done
    return 1
}

start_target() {
    local target="$1"
    local scheme="$2"
    local protocol="$3"
    printf 'Starting %s (%s/%s)...\n' "$target" "$scheme" "$protocol"
    "${COMPOSE[@]}" up -d "$target"
    probe_target "$target" "$scheme" "$protocol" || die "$target did not return a successful health response"
}

stop_target() {
    local target="$1"
    "${COMPOSE[@]}" stop "$target" >/dev/null
}

start_monitor() {
    local container_id="$1"
    local stats_file="$2"
    printf 'timestamp\tcpu_percent\tmemory_usage\n' >"$stats_file"
    (
        while true; do
            line="$(docker stats --no-stream --format '{{.CPUPerc}}\t{{.MemUsage}}' "$container_id" 2>/dev/null || true)"
            if [[ -n "$line" ]]; then
                printf '%s\t%s\n' "$(date +%s.%N)" "$line"
            fi
            sleep 0.25
        done
    ) >>"$stats_file" &
    MONITOR_PID=$!
}

run_oha() {
    local target="$1"
    local scheme="$2"
    local protocol="$3"
    local path="$4"
    local load_model="$5"
    local load="$6"
    local connections="$7"
    local http2_parallel="$8"
    local duration="$9"
    local flags_csv="${10}"
    local result_path="${11}"
    local stderr_path="${12}"

    local port=8080
    if [[ "$scheme" == "https" ]]; then
        port=8443
    fi
    local url
    url="$(target_url "$target" "$scheme" "$path")"
    local connection_count="$connections"
    if [[ "$load_model" == "concurrency" ]]; then
        connection_count="$load"
    fi
    local args=(
        --no-tui
        --no-color
        --stats-success-breakdown
        --output-format json
        -z "${duration}s"
        -c "$connection_count"
        --connect-to "bench.local:${port}:${target}:${port}"
        --host bench.local
    )
    if [[ "$load_model" == "qps" ]]; then
        args+=(-q "$load")
    fi
    if [[ "$scheme" == "https" ]]; then
        args+=(--insecure)
    fi
    if [[ "$protocol" == "http2" ]]; then
        args+=(--http2)
        if (( http2_parallel > 1 )); then
            args+=(-p "$http2_parallel")
        fi
    fi
    if [[ -n "$flags_csv" ]]; then
        local flags=()
        IFS=',' read -r -a flags <<<"$flags_csv"
        args+=("${flags[@]}")
    fi
    args+=("$url")

    set +e
    "${COMPOSE[@]}" run --rm --no-deps loadgen "${args[@]}" >"$result_path" 2>"$stderr_path"
    local status=$?
    set -e
    return "$status"
}

run_plan_point() {
    local scenario_id="$1"
    local scheme="$2"
    local protocol="$3"
    local path="$4"
    local load_model="$5"
    local load="$6"
    local connections="$7"
    local http2_parallel="$8"
    local warmup="$9"
    local duration="${10}"
    local repetitions="${11}"
    local flags_csv="${12}"
    local reference_load="${13}"
    local target

    for target in nginx caddy raddex; do
        start_target "$target" "$scheme" "$protocol"
        local container_id
        container_id="$("${COMPOSE[@]}" ps -q "$target")"
        [[ -n "$container_id" ]] || die "could not resolve the container id for $target"

        for repetition in $(seq 1 "$repetitions"); do
            local point_dir="$RAW_DIR/$scenario_id/$target/${load_model}-${load}"
            mkdir -p "$point_dir"
            local warmup_log="$point_dir/rep-${repetition}.warmup.log"
            local result_path="$point_dir/rep-${repetition}.json"
            local stderr_path="$point_dir/rep-${repetition}.stderr.log"
            local stats_path="$point_dir/rep-${repetition}.stats.tsv"
            local meta_path="$point_dir/rep-${repetition}.meta.json"

            if (( warmup > 0 )); then
                run_oha "$target" "$scheme" "$protocol" "$path" "$load_model" "$load" "$connections" "$http2_parallel" "$warmup" "$flags_csv" /dev/null "$warmup_log" || die "warmup failed for $scenario_id / $target"
            fi

            start_monitor "$container_id" "$stats_path"
            set +e
            run_oha "$target" "$scheme" "$protocol" "$path" "$load_model" "$load" "$connections" "$http2_parallel" "$duration" "$flags_csv" "$result_path" "$stderr_path"
            local exit_code=$?
            set -e
            cleanup_monitor
            printf '{"scenario_id":"%s","target":"%s","load_model":"%s","load":%s,"reference_load":%s,"repetition":%s,"exit_code":%s}\n' \
                "$scenario_id" "$target" "$load_model" "$load" "$reference_load" "$repetition" "$exit_code" >"$meta_path"
            (( exit_code == 0 )) || die "load generator failed for $scenario_id / $target / rep-${repetition}"
        done
        stop_target "$target"
    done
}

mapfile -t PLAN_LINES <"$PLAN_FILE"
for plan_line in "${PLAN_LINES[@]}"; do
    IFS='|' read -r scenario_id scheme protocol path load_model load connections http2_parallel warmup duration repetitions flags_csv reference_load <<<"$plan_line"
    [[ -z "$scenario_id" ]] && continue
    printf 'Running %s at %s=%s...\n' "$scenario_id" "$load_model" "$load"
    run_plan_point "$scenario_id" "$scheme" "$protocol" "$path" "$load_model" "$load" "$connections" "$http2_parallel" "$warmup" "$duration" "$repetitions" "$flags_csv" "$reference_load"
done

"${COMPOSE[@]}" run --rm --no-deps reporter /bench/scripts/collect.py \
    --collect \
    --bench-root /bench \
    --raw-dir "/bench/results/$RUN_ID/raw" \
    --scenario-file /bench/scenarios/scenarios.json \
    --profile "$MODE" \
    --manifest "/bench/results/$RUN_ID/run.json" \
    --summary-out "/bench/results/$RUN_ID/summary.json"

"${COMPOSE[@]}" run --rm --no-deps reporter /bench/scripts/report.py \
    --summary "/bench/results/$RUN_ID/summary.json" \
    --output-dir "/bench/results/$RUN_ID" \
    --public-dir /public/benchmarks

printf '\nBenchmark complete.\nRun directory: %s\nMarkdown report: %s/report.md\nHTML report: %s/report.html\nPublic charts: %s\n' \
    "$RUN_DIR" "$RUN_DIR" "$RUN_DIR" "$ROOT_DIR/page/public/benchmarks"
