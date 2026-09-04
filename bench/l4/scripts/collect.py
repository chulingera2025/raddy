#!/usr/bin/env python3
"""Plan and aggregate the Linux-only L4 forwarding benchmark."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import statistics
import sys
from pathlib import Path
from typing import Any, Iterable


TARGETS = ("nginx", "caddy", "raddex-l4", "nat")
MODES = {
    "tcp-throughput",
    "tcp-latency",
    "tcp-connections",
    "tcp-connect-rate",
    "udp-throughput",
    "udp-latency",
    "udp-flows",
}
KINDS = {
    "tcp_throughput",
    "udp_throughput",
    "udp_pps",
    "p99_latency",
    "tcp_connect_rate",
    "tcp_long_lived",
    "udp_flows",
}
CONNECTION_MODES = {"tcp-connections", "tcp-connect-rate", "udp-flows"}
CONNECTION_KINDS = {"tcp_connect_rate", "tcp_long_lived", "udp_flows"}
TRANSPORTS = {"tcp", "udp"}
NUMERIC_FIELDS = (
    "elapsed_seconds",
    "establishment_seconds",
    "requested_connections",
    "successful_connections",
    "failed_connections",
    "completed_operations",
    "sent_bytes",
    "received_bytes",
    "sent_packets",
    "received_packets",
    "errors",
    "success_rate",
    "error_rate",
    "packet_loss_pct",
    "throughput_mbps",
    "packets_per_second",
    "connection_rate_per_second",
    "offered_packets_per_second",
    "p50_latency_us",
    "p95_latency_us",
    "p99_latency_us",
    "max_latency_us",
    "p50_connect_latency_us",
    "p95_connect_latency_us",
    "p99_connect_latency_us",
    "max_connect_latency_us",
)


def _positive_integer(value: Any) -> bool:
    """Return whether a configuration value is a positive integer."""

    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def load_json(path: Path) -> dict[str, Any]:
    """Load a JSON object from a file.

    Args:
        path: JSON file to load.

    Returns:
        The decoded object.

    Raises:
        ValueError: If the file does not contain an object.
        OSError: If the file cannot be read.
    """

    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object in {path}")
    return value


def parse_versions(path: Path) -> dict[str, str]:
    """Parse a simple environment-style versions file.

    Args:
        path: File containing ``KEY=value`` entries.

    Returns:
        A mapping of keys to values.

    Raises:
        ValueError: If a non-comment line is malformed.
        OSError: If the file cannot be read.
    """

    values: dict[str, str] = {}
    for line_number, raw_line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            raise ValueError(f"invalid versions entry at {path}:{line_number}")
        name, value = line.split("=", 1)
        values[name.strip()] = value.strip()
    return values


def sha256_file(path: Path) -> str:
    """Return the SHA-256 digest of a file.

    Args:
        path: File to hash.

    Returns:
        Lowercase hexadecimal digest.

    Raises:
        OSError: If the file cannot be read.
    """

    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_config(config: dict[str, Any]) -> None:
    """Validate the L4 scenario contract.

    Args:
        config: Decoded ``scenarios.json`` content.

    Returns:
        None.

    Raises:
        ValueError: If a profile, scenario, mode, or numeric bound is invalid.
    """

    defaults = config.get("defaults")
    scenarios = config.get("scenarios")
    profiles = config.get("profiles")
    if not isinstance(defaults, dict) or not isinstance(scenarios, list) or not isinstance(profiles, dict):
        raise ValueError("configuration requires defaults, profiles, and scenarios")
    for field in ("warmup_seconds", "duration_seconds", "repetitions", "connect_timeout_ms"):
        if not _positive_integer(defaults.get(field)):
            raise ValueError(f"defaults.{field} must be a positive integer")
    stable_error_rate = defaults.get("stable_error_rate")
    if isinstance(stable_error_rate, bool) or not isinstance(stable_error_rate, (int, float)) or not 0 <= stable_error_rate < 1:
        raise ValueError("defaults.stable_error_rate must be in [0, 1)")

    ids: set[str] = set()
    for scenario in scenarios:
        if not isinstance(scenario, dict):
            raise ValueError("every scenario must be an object")
        required = ("id", "title", "kind", "mode", "transport", "port", "connections", "payload_bytes", "window")
        missing = [field for field in required if field not in scenario]
        if missing:
            raise ValueError(f"scenario is missing fields: {', '.join(missing)}")
        scenario_id = scenario["id"]
        if not isinstance(scenario_id, str) or not scenario_id or scenario_id in ids:
            raise ValueError(f"scenario id must be unique and non-empty: {scenario_id!r}")
        ids.add(scenario_id)
        if scenario["mode"] not in MODES:
            raise ValueError(f"unsupported mode for {scenario_id}: {scenario['mode']}")
        if scenario["kind"] not in KINDS:
            raise ValueError(f"unsupported kind for {scenario_id}: {scenario['kind']}")
        if scenario["transport"] not in TRANSPORTS:
            raise ValueError(f"unsupported transport for {scenario_id}")
        if scenario["mode"].startswith("tcp-") and scenario["transport"] != "tcp":
            raise ValueError(f"TCP mode has non-TCP transport for {scenario_id}")
        if scenario["mode"].startswith("udp-") and scenario["transport"] != "udp":
            raise ValueError(f"UDP mode has non-UDP transport for {scenario_id}")
        for field in ("port", "connections", "payload_bytes", "window"):
            if not _positive_integer(scenario[field]):
                raise ValueError(f"{scenario_id}.{field} must be a positive integer")
        if scenario["port"] > 65_535:
            raise ValueError(f"{scenario_id}.port exceeds the TCP/UDP port range")
        if scenario["payload_bytes"] * scenario["window"] > 8 * 1024 * 1024:
            raise ValueError(f"{scenario_id}.payload_bytes * window exceeds the 8 MiB loadgen limit")
        if scenario["connections"] > 50_000:
            raise ValueError(f"{scenario_id}.connections exceeds the 50K benchmark ceiling")
        if scenario["transport"] == "udp" and scenario["payload_bytes"] > 65_507:
            raise ValueError(f"{scenario_id}.payload_bytes exceeds the UDP payload ceiling")
        if scenario["mode"] in CONNECTION_MODES:
            rate = scenario.get("connect_rate", 0)
            if not _positive_integer(rate):
                raise ValueError(f"{scenario_id}.connect_rate must be positive for connection modes")
        if scenario["mode"] == "udp-throughput":
            rate = scenario.get("packets_per_second", 0)
            if not _positive_integer(rate):
                raise ValueError(f"{scenario_id}.packets_per_second must be positive for UDP throughput")
        for field in ("warmup_seconds", "duration_seconds", "repetitions", "connect_timeout_ms", "connect_rate"):
            if field in scenario and not _positive_integer(scenario[field]):
                raise ValueError(f"{scenario_id}.{field} must be a positive integer")

    for profile_name, profile in profiles.items():
        if not isinstance(profile, dict):
            raise ValueError(f"profile {profile_name} must be an object")
        selected = profile.get("scenario_ids", "all")
        if selected != "all" and (not isinstance(selected, list) or any(item not in ids for item in selected)):
            raise ValueError(f"profile {profile_name} contains an unknown scenario")
        for field in ("warmup_seconds", "duration_seconds", "repetitions"):
            if field in profile and not _positive_integer(profile[field]):
                raise ValueError(f"profile {profile_name}.{field} must be a positive integer")


def iter_plan(config: dict[str, Any], profile_name: str) -> list[dict[str, Any]]:
    """Expand one profile into executable scenario records.

    Args:
        config: Decoded and validated scenario configuration.
        profile_name: Profile name such as ``quick`` or ``full``.

    Returns:
        Ordered scenario records for the runner.

    Raises:
        ValueError: If the profile does not exist or selects no scenarios.
    """

    validate_config(config)
    defaults = config["defaults"]
    profile = config["profiles"].get(profile_name)
    if not isinstance(profile, dict):
        raise ValueError(f"unknown benchmark profile: {profile_name}")
    selected_ids = profile.get("scenario_ids", "all")
    selected = {item["id"] for item in config["scenarios"] if selected_ids == "all" or item["id"] in selected_ids}
    plans: list[dict[str, Any]] = []
    for scenario in config["scenarios"]:
        if scenario["id"] not in selected:
            continue
        plans.append(
            {
                **scenario,
                "warmup_seconds": profile.get(
                    "warmup_seconds", scenario.get("warmup_seconds", defaults["warmup_seconds"])
                ),
                "duration_seconds": profile.get(
                    "duration_seconds", scenario.get("duration_seconds", defaults["duration_seconds"])
                ),
                "repetitions": profile.get(
                    "repetitions", scenario.get("repetitions", defaults["repetitions"])
                ),
                "connect_timeout_ms": scenario.get("connect_timeout_ms", defaults["connect_timeout_ms"]),
                "connect_rate": scenario.get("connect_rate", 0),
                "packets_per_second": scenario.get("packets_per_second", 0),
            }
        )
    if not plans:
        raise ValueError(f"profile {profile_name} selected no scenarios")
    return plans


def print_plan(config: dict[str, Any], profile_name: str) -> None:
    """Print a pipe-delimited execution plan for the shell runner.

    Args:
        config: Decoded scenario configuration.
        profile_name: Profile to expand.

    Returns:
        None. One line is written for each scenario.

    Raises:
        ValueError: If a field contains the pipe delimiter.
    """

    fields = (
        "id",
        "title",
        "kind",
        "mode",
        "transport",
        "port",
        "connections",
        "payload_bytes",
        "window",
        "connect_rate",
        "packets_per_second",
        "warmup_seconds",
        "duration_seconds",
        "repetitions",
        "connect_timeout_ms",
    )
    for plan in iter_plan(config, profile_name):
        values = [str(plan[field]) for field in fields]
        if any("|" in value for value in values):
            raise ValueError("scenario values must not contain '|'")
        print("|".join(values))


def parse_memory_bytes(value: str) -> float:
    """Parse a Docker memory value into bytes.

    Args:
        value: Value such as ``12.5MiB`` or ``1GiB``.

    Returns:
        Memory size in bytes.

    Raises:
        ValueError: If the value is not a supported quantity.
    """

    units = {
        "B": 1,
        "KB": 1000,
        "MB": 1000**2,
        "GB": 1000**3,
        "KIB": 1024,
        "MIB": 1024**2,
        "GIB": 1024**3,
    }
    stripped = value.strip().replace(" ", "")
    number = ""
    unit = ""
    for char in stripped:
        if char.isdigit() or char == ".":
            number += char
        else:
            unit += char
    if not number or unit.upper() not in units:
        raise ValueError(f"invalid memory quantity: {value!r}")
    return float(number) * units[unit.upper()]


def parse_stats(path: Path, require_nat_state: bool = False) -> dict[str, float]:
    """Aggregate Docker, cgroup, and optional conntrack samples.

    Args:
        path: Tab-separated stats file from the runner monitor.
        require_nat_state: Require conntrack and slab samples for the NAT target.

    Returns:
        Average and peak CPU, peak memory, cgroup accounting, and peak NAT
        table counters. Cgroup fields are zero when the host exposes no
        readable cgroup v2 files.

    Raises:
        ValueError: If no CPU/memory samples are valid or NAT state is required
            but missing.
        OSError: If the file cannot be read.
    """

    cpu_values: list[float] = []
    memory_values: list[float] = []
    conntrack_values: list[float] = []
    nf_values: list[float] = []
    nf_bytes_values: list[float] = []
    cgroup_memory_current: list[float] = []
    cgroup_memory_peak: list[float] = []
    cgroup_memory_anon: list[float] = []
    cgroup_memory_file: list[float] = []
    cgroup_memory_kernel: list[float] = []
    cgroup_memory_sock: list[float] = []
    cgroup_pids: list[float] = []
    cgroup_threads: list[float] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("timestamp\t"):
            continue
        fields = line.split("\t")
        if len(fields) < 3:
            continue
        try:
            cpu_values.append(float(fields[1].rstrip("%")))
            memory_values.append(parse_memory_bytes(fields[2].split("/", 1)[0]))
            if len(fields) >= 4 and fields[3]:
                conntrack_values.append(float(fields[3]))
            if len(fields) >= 5 and fields[4]:
                nf_values.append(float(fields[4]))
            if len(fields) >= 6 and fields[5]:
                nf_bytes_values.append(float(fields[5]))
            if len(fields) >= 7 and fields[6]:
                cgroup_memory_current.append(float(fields[6]))
            if len(fields) >= 8 and fields[7]:
                cgroup_memory_peak.append(float(fields[7]))
            if len(fields) >= 9 and fields[8]:
                cgroup_memory_anon.append(float(fields[8]))
            if len(fields) >= 10 and fields[9]:
                cgroup_memory_file.append(float(fields[9]))
            if len(fields) >= 11 and fields[10]:
                cgroup_memory_kernel.append(float(fields[10]))
            if len(fields) >= 12 and fields[11]:
                cgroup_memory_sock.append(float(fields[11]))
            if len(fields) >= 13 and fields[12]:
                cgroup_pids.append(float(fields[12]))
            if len(fields) >= 14 and fields[13]:
                cgroup_threads.append(float(fields[13]))
        except ValueError:
            continue
    if not cpu_values or not memory_values:
        raise ValueError(f"no valid Docker stats samples in {path}")
    if require_nat_state and (not conntrack_values or not nf_values or not nf_bytes_values):
        raise ValueError(f"missing NAT conntrack/slab samples in {path}")
    return {
        "cpu_avg_percent": statistics.fmean(cpu_values),
        "cpu_peak_percent": max(cpu_values),
        "memory_peak_bytes": max(memory_values),
        "conntrack_peak": max(conntrack_values, default=0.0),
        "nf_conntrack_objects_peak": max(nf_values, default=0.0),
        "nf_conntrack_bytes_peak": max(nf_bytes_values, default=0.0),
        "memory_current_peak": max(cgroup_memory_current, default=0.0),
        "memory_peak_cgroup": max(cgroup_memory_peak, default=0.0),
        "memory_anon_peak": max(cgroup_memory_anon, default=0.0),
        "memory_file_peak": max(cgroup_memory_file, default=0.0),
        "memory_kernel_peak": max(cgroup_memory_kernel, default=0.0),
        "memory_sock_peak": max(cgroup_memory_sock, default=0.0),
        "pids_current_peak": max(cgroup_pids, default=0.0),
        "threads_current_peak": max(cgroup_threads, default=0.0),
    }


def parse_host_delta(path: Path) -> dict[str, float]:
    """Calculate host CPU jiffy deltas captured around one load.

    Args:
        path: Two-line host snapshot containing total, idle, and softirq jiffies.

    Returns:
        Total and softirq CPU milliseconds over the measurement interval.

    Raises:
        ValueError: If the snapshot is incomplete or counters decrease.
        OSError: If the file cannot be read.
    """

    rows = []
    for line in path.read_text(encoding="utf-8").splitlines():
        fields = line.split("\t")
        if len(fields) == 4:
            rows.append(tuple(float(item) for item in fields))
    if len(rows) != 2:
        raise ValueError(f"host snapshot must contain two rows: {path}")
    before, after = rows
    total_delta = after[1] - before[1]
    softirq_delta = after[3] - before[3]
    if total_delta < 0 or softirq_delta < 0:
        raise ValueError(f"host CPU snapshot counters decreased: {path}")
    hz = float(os.sysconf(os.sysconf_names["SC_CLK_TCK"]))
    return {
        "host_cpu_ms": total_delta / hz * 1000.0,
        "host_softirq_ms": softirq_delta / hz * 1000.0,
    }


def parse_loadgen(path: Path) -> dict[str, Any]:
    """Load and validate one load-generator JSON result.

    Args:
        path: JSON file emitted by ``l4-bench-loadgen``.

    Returns:
        The validated result object.

    Raises:
        ValueError: If required numeric fields are missing or invalid.
        OSError: If the file cannot be read.
    """

    payload = load_json(path)
    if payload.get("schema_version") != 1:
        raise ValueError(f"unsupported loadgen schema in {path}")
    for field in NUMERIC_FIELDS:
        if field not in payload:
            raise ValueError(f"missing loadgen field {field} in {path}")
        if payload[field] is None:
            continue
        try:
            number = float(payload[field])
        except (TypeError, ValueError) as error:
            raise ValueError(f"invalid loadgen field {field} in {path}") from error
        if not math.isfinite(number):
            raise ValueError(f"non-finite loadgen field {field} in {path}")
    return payload


def median(values: Iterable[float]) -> float:
    """Return the median of a non-empty iterable.

    Args:
        values: Numeric values.

    Returns:
        The median as a float.

    Raises:
        ValueError: If the iterable is empty.
    """

    values = list(values)
    if not values:
        raise ValueError("cannot take the median of an empty sequence")
    return float(statistics.median(values))


def build_manifest(
    bench_root: Path,
    run_id: str,
    profile: str,
    raddex_commit: str,
    runtime_parameters: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Build a reproducibility manifest without host identity.

    Args:
        bench_root: Mounted L4 benchmark directory.
        run_id: Unique run identifier.
        profile: Selected benchmark profile.
        raddex_commit: Raddex commit used for the target image.
        runtime_parameters: Resource and runner settings used for the run.

    Returns:
        Manifest with tool versions, input hashes, and runtime parameters.

    Raises:
        OSError: If a configuration or versions file cannot be read.
    """

    hashes: dict[str, str] = {}
    for target in ("nginx", "caddy", "raddex", "nftables"):
        directory = bench_root / "configs" / target
        for path in sorted(directory.iterdir()):
            if path.is_file():
                hashes[f"configs/{target}/{path.name}"] = sha256_file(path)
    hashes["router/router.sh"] = sha256_file(bench_root / "router" / "router.sh")
    hashes["compose.yaml"] = sha256_file(bench_root / "compose.yaml")
    hashes["scenarios/scenarios.json"] = sha256_file(bench_root / "scenarios" / "scenarios.json")
    input_hashes: dict[str, str] = {}
    for path in sorted(bench_root.rglob("*")):
        relative = path.relative_to(bench_root)
        # The stable overview is generated from a prior run and must not make
        # the next run's input fingerprint depend on that output artifact.
        if (
            path.is_file()
            and "results" not in relative.parts
            and "__pycache__" not in relative.parts
            and "target" not in relative.parts
            and relative.as_posix() != "overview.svg"
        ):
            input_hashes[relative.as_posix()] = sha256_file(path)
    return {
        "schema_version": 1,
        "run_id": run_id,
        "profile": profile,
        "raddex_commit": raddex_commit,
        "targets": list(TARGETS),
        "kernel_release": platform.release(),
        "tools": parse_versions(bench_root / "versions.env"),
        "config_sha256": hashes,
        "input_sha256": input_hashes,
        "runtime": runtime_parameters or {},
    }


def derive_measurement(
    plan: dict[str, Any],
    target: str,
    loadgen: dict[str, Any],
    stats: dict[str, float],
    host: dict[str, float],
) -> dict[str, Any]:
    """Combine one loadgen result with resource samples.

    Args:
        plan: Scenario plan record.
        target: Logical target name.
        loadgen: Parsed load-generator result.
        stats: Parsed Docker resource samples.
        host: Host CPU delta captured around the load.

    Returns:
        One raw measurement with derived CPU costs.

    Raises:
        ValueError: If the result cannot produce a positive elapsed duration.
    """

    elapsed = float(loadgen["elapsed_seconds"])
    if elapsed <= 0:
        raise ValueError(f"non-positive elapsed time for {plan['id']} / {target}")
    successful_connections = float(loadgen["successful_connections"])
    completed_operations = float(loadgen["completed_operations"])
    if target == "nat":
        # NAT packet work is performed by the host kernel, not by the idle
        # router process cgroup. Softirq time is the closest isolated signal
        # available without claiming that conntrack memory is process RSS.
        cpu_ms = host["host_softirq_ms"]
        cpu_basis = "host_softirq"
    else:
        cpu_ms = float(stats["cpu_avg_percent"]) / 100.0 * elapsed * 1000.0
        cpu_basis = "target_cgroup"
    return {
        "scenario_id": plan["id"],
        "title": plan["title"],
        "kind": plan["kind"],
        "mode": plan["mode"],
        "transport": plan["transport"],
        "target": target,
        "connections": plan["connections"],
        "payload_bytes": plan["payload_bytes"],
        "metrics": {
            **{field: loadgen.get(field) for field in NUMERIC_FIELDS},
            "cpu_avg_percent": stats["cpu_avg_percent"],
            "cpu_peak_percent": stats["cpu_peak_percent"],
            "peak_memory_bytes": stats["memory_peak_bytes"],
            "peak_conntrack_count": stats["conntrack_peak"],
            "peak_nf_conntrack_objects": stats["nf_conntrack_objects_peak"],
            "peak_nf_conntrack_bytes": stats["nf_conntrack_bytes_peak"],
            "cgroup_memory_current_peak": stats["memory_current_peak"],
            "cgroup_memory_peak_bytes": stats["memory_peak_cgroup"],
            "cgroup_memory_anon_peak_bytes": stats["memory_anon_peak"],
            "cgroup_memory_file_peak_bytes": stats["memory_file_peak"],
            "cgroup_memory_kernel_peak_bytes": stats["memory_kernel_peak"],
            "cgroup_memory_sock_peak_bytes": stats["memory_sock_peak"],
            "cgroup_pids_peak": stats["pids_current_peak"],
            "cgroup_threads_peak": stats["threads_current_peak"],
            "host_cpu_ms": host["host_cpu_ms"],
            "host_softirq_ms": host["host_softirq_ms"],
            "cpu_ms_total": cpu_ms,
            "cpu_ms_per_operation": cpu_ms / completed_operations if completed_operations > 0 else None,
            "cpu_ms_per_connection": cpu_ms / successful_connections if successful_connections > 0 else None,
            "cpu_basis": cpu_basis,
            "p99_latency_ms": (
                float(loadgen["p99_latency_us"]) / 1000.0
                if loadgen.get("p99_latency_us") is not None
                else None
            ),
            "p99_connect_latency_ms": (
                float(loadgen["p99_connect_latency_us"]) / 1000.0
                if loadgen.get("p99_connect_latency_us") is not None
                else None
            ),
        },
    }


def aggregate_measurements(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Aggregate repeated target measurements by median.

    Args:
        records: Raw derived records, one per repetition.

    Returns:
        One median record per scenario and target.

    Raises:
        ValueError: If records are empty or a required metric is missing.
    """

    if not records:
        raise ValueError("no L4 measurements were found")
    grouped: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for record in records:
        grouped.setdefault((record["scenario_id"], record["target"]), []).append(record)
    output: list[dict[str, Any]] = []
    metric_fields = (
        "elapsed_seconds",
        "establishment_seconds",
        "requested_connections",
        "successful_connections",
        "failed_connections",
        "completed_operations",
        "sent_bytes",
        "received_bytes",
        "sent_packets",
        "received_packets",
        "errors",
        "success_rate",
        "error_rate",
        "packet_loss_pct",
        "throughput_mbps",
        "packets_per_second",
        "connection_rate_per_second",
        "offered_packets_per_second",
        "p50_latency_us",
        "p95_latency_us",
        "p99_latency_us",
        "max_latency_us",
        "p50_connect_latency_us",
        "p95_connect_latency_us",
        "p99_connect_latency_us",
        "max_connect_latency_us",
        "cpu_avg_percent",
        "cpu_peak_percent",
        "peak_memory_bytes",
        "peak_conntrack_count",
        "peak_nf_conntrack_objects",
        "peak_nf_conntrack_bytes",
        "host_cpu_ms",
        "host_softirq_ms",
        "cpu_ms_total",
        "cpu_ms_per_operation",
        "cpu_ms_per_connection",
        "p99_latency_ms",
        "p99_connect_latency_ms",
        "cgroup_memory_current_peak",
        "cgroup_memory_peak_bytes",
        "cgroup_memory_anon_peak_bytes",
        "cgroup_memory_file_peak_bytes",
        "cgroup_memory_kernel_peak_bytes",
        "cgroup_memory_sock_peak_bytes",
        "cgroup_pids_peak",
        "cgroup_threads_peak",
    )
    for (scenario_id, target), group in grouped.items():
        first = group[0]
        metrics: dict[str, Any] = {}
        for field in metric_fields:
            values = [record["metrics"].get(field) for record in group]
            if any(value is None for value in values):
                metrics[field] = None
            else:
                metrics[field] = median(float(value) for value in values)
        metrics["cpu_basis"] = first["metrics"]["cpu_basis"]
        output.append(
            {
                "scenario_id": scenario_id,
                "title": first["title"],
                "kind": first["kind"],
                "mode": first["mode"],
                "transport": first["transport"],
                "target": target,
                "connections": first["connections"],
                "payload_bytes": first["payload_bytes"],
                "replicates": len(group),
                "metrics": metrics,
            }
        )
    return sorted(output, key=lambda item: (item["scenario_id"], TARGETS.index(item["target"])))


def normalize_measurements(measurements: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Normalize comparable L4 metrics against the Nginx record.

    Args:
        measurements: Median measurements with all configured targets present.

    Returns:
        Normalized records with percentage metrics and absolute error fields.

    Raises:
        ValueError: If a scenario lacks a target or has an invalid baseline.
    """

    grouped: dict[str, dict[str, dict[str, Any]]] = {}
    for item in measurements:
        grouped.setdefault(item["scenario_id"], {})[item["target"]] = item
    normalized: list[dict[str, Any]] = []
    for scenario_id, targets in grouped.items():
        missing = [target for target in TARGETS if target not in targets]
        if missing:
            raise ValueError(f"missing targets for {scenario_id}: {', '.join(missing)}")
        baseline = targets["nginx"]["metrics"]

        def ratio(target: str, field: str) -> float:
            value = targets[target]["metrics"].get(field)
            base = baseline.get(field)
            if (
                value is None
                or base is None
                or not math.isfinite(float(value))
                or not math.isfinite(float(base))
                or float(base) <= 0
            ):
                raise ValueError(f"cannot normalize {field} for {scenario_id} / {target}")
            return float(value) / float(base) * 100.0

        def required_absolute(target: str, field: str) -> float:
            value = targets[target]["metrics"].get(field)
            if value is None or not math.isfinite(float(value)) or float(value) < 0:
                raise ValueError(f"missing or invalid {field} for {scenario_id} / {target}")
            return float(value)

        kind = targets["nginx"]["kind"]
        for target in TARGETS:
            metrics = targets[target]["metrics"]
            relative: dict[str, Any] = {
                "throughput_pct": None,
                "pps_pct": None,
                "connection_rate_pct": None,
                "active_connections_pct": None,
                "p99_latency_pct": None,
                "cpu_pct": None
                if target == "nat"
                else ratio(target, "cpu_ms_per_connection" if kind in CONNECTION_KINDS else "cpu_ms_per_operation"),
                "memory_pct": None if target == "nat" else ratio(target, "peak_memory_bytes"),
                "error_rate": required_absolute(target, "error_rate"),
                "packet_loss_pct": required_absolute(target, "packet_loss_pct"),
            }
            if kind in {"tcp_throughput", "udp_throughput"}:
                relative["throughput_pct"] = ratio(target, "throughput_mbps")
            if kind == "udp_pps":
                relative["pps_pct"] = ratio(target, "packets_per_second")
            if kind == "tcp_connect_rate":
                relative["connection_rate_pct"] = ratio(target, "connection_rate_per_second")
            if kind in {"tcp_long_lived", "udp_flows"}:
                relative["active_connections_pct"] = ratio(target, "successful_connections")
            if kind == "p99_latency":
                relative["p99_latency_pct"] = ratio(target, "p99_latency_ms")
            normalized.append(
                {
                    "scenario_id": scenario_id,
                    "title": targets[target]["title"],
                    "kind": kind,
                    "mode": targets[target]["mode"],
                    "transport": targets[target]["transport"],
                    "target": target,
                    "connections": targets[target]["connections"],
                    "payload_bytes": targets[target]["payload_bytes"],
                    "raw": metrics,
                    "metrics": relative,
                }
            )
    return sorted(normalized, key=lambda item: (item["scenario_id"], TARGETS.index(item["target"])))


def collect_results(
    bench_root: Path,
    raw_dir: Path,
    scenario_file: Path,
    profile_name: str,
    manifest: dict[str, Any],
) -> dict[str, Any]:
    """Read all raw files and build a normalized summary.

    Args:
        bench_root: Mounted benchmark root.
        raw_dir: Per-run raw result directory.
        scenario_file: Scenario configuration file.
        profile_name: Profile used for the run.
        manifest: Reproducibility manifest.

    Returns:
        Summary containing raw medians and normalized records.

    Raises:
        ValueError: If raw files are incomplete, failed, or inconsistent.
        OSError: If a raw file cannot be read.
    """

    config = load_json(scenario_file)
    plans = iter_plan(config, profile_name)
    records: list[dict[str, Any]] = []
    for plan in plans:
        for target in TARGETS:
            point_dir = raw_dir / plan["id"] / target
            for repetition in range(1, int(plan["repetitions"]) + 1):
                result_path = point_dir / f"rep-{repetition}.json"
                stats_path = point_dir / f"rep-{repetition}.stats.tsv"
                host_path = point_dir / f"rep-{repetition}.host.tsv"
                meta_path = point_dir / f"rep-{repetition}.meta.json"
                required = (result_path, stats_path, host_path, meta_path)
                if any(not path.is_file() for path in required):
                    raise ValueError(f"incomplete raw result for {plan['id']} / {target} / rep-{repetition}")
                meta = load_json(meta_path)
                if int(meta.get("exit_code", 1)) != 0:
                    raise ValueError(f"load generator failed for {plan['id']} / {target} / rep-{repetition}")
                loadgen = parse_loadgen(result_path)
                stats = parse_stats(stats_path, target == "nat")
                host = parse_host_delta(host_path)
                records.append(derive_measurement(plan, target, loadgen, stats, host))
    measurements = aggregate_measurements(records)
    normalized = normalize_measurements(measurements)
    return {
        "schema_version": 1,
        "manifest": manifest,
        "scenario_file": str(scenario_file),
        "measurements": measurements,
        "normalized": normalized,
    }


def parse_args() -> argparse.Namespace:
    """Parse collection and planning command-line arguments.

    Returns:
        Parsed command-line namespace.

    Raises:
        SystemExit: If argparse rejects the command line.
    """

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario-file", type=Path, default=Path("/bench-l4/scenarios/scenarios.json"))
    parser.add_argument("--profile", default="quick")
    parser.add_argument("--plan-shell", action="store_true")
    parser.add_argument("--write-manifest", action="store_true")
    parser.add_argument("--collect", action="store_true")
    parser.add_argument("--bench-root", type=Path, default=Path("/bench-l4"))
    parser.add_argument("--raw-dir", type=Path)
    parser.add_argument("--run-id")
    parser.add_argument("--raddex-commit", default="unknown")
    parser.add_argument("--cpu-limit", default="unknown")
    parser.add_argument("--memory-limit", default="unknown")
    parser.add_argument("--raddex-threads", type=int)
    parser.add_argument("--loadgen-shards", type=int)
    parser.add_argument("--perf-system-wide", type=int, choices=(0, 1))
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--summary-out", type=Path)
    return parser.parse_args()


def main() -> int:
    """Run the requested planning, manifest, or collection operation.

    Returns:
        Zero on success, otherwise one after printing a diagnostic.

    Raises:
        None: Expected input errors are converted into a non-zero status.
    """

    args = parse_args()
    try:
        config = load_json(args.scenario_file)
        if args.plan_shell:
            print_plan(config, args.profile)
            return 0
        if args.write_manifest:
            if not args.run_id:
                raise ValueError("--run-id is required with --write-manifest")
            manifest = build_manifest(
                args.bench_root,
                args.run_id,
                args.profile,
                args.raddex_commit,
                {
                    "cpu_limit": args.cpu_limit,
                    "memory_limit": args.memory_limit,
                    "raddex_threads": args.raddex_threads,
                    "loadgen_shards": args.loadgen_shards,
                    "perf_system_wide": args.perf_system_wide,
                },
            )
            output = args.bench_root / "results" / args.run_id / "run.json"
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            return 0
        if args.collect:
            if args.raw_dir is None or args.manifest is None or args.summary_out is None:
                raise ValueError("--raw-dir, --manifest, and --summary-out are required with --collect")
            summary = collect_results(
                args.bench_root,
                args.raw_dir,
                args.scenario_file,
                args.profile,
                load_json(args.manifest),
            )
            args.summary_out.parent.mkdir(parents=True, exist_ok=True)
            args.summary_out.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            return 0
        raise ValueError("choose one of --plan-shell, --write-manifest, or --collect")
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"collect error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
