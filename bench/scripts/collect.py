#!/usr/bin/env python3
"""Plan benchmark runs and aggregate raw oha and Docker statistics."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import statistics
import sys
from pathlib import Path
from typing import Any, Iterable


TARGETS = ("nginx", "caddy", "raddex")
NUMERIC_METRICS = (
    "qps",
    "success_rate",
    "error_rate",
    "p50_ms",
    "p95_ms",
    "p99_ms",
    "max_ms",
    "cpu_avg_percent",
    "cpu_peak_percent",
    "memory_peak_bytes",
    "cpu_ms_per_request",
)
MEMORY_UNITS = {
    "B": 1,
    "KB": 1000,
    "MB": 1000**2,
    "GB": 1000**3,
    "TB": 1000**4,
    "KIB": 1024,
    "MIB": 1024**2,
    "GIB": 1024**3,
    "TIB": 1024**4,
}


def load_json(path: Path) -> dict[str, Any]:
    """Load a JSON object from ``path``.

    Args:
        path: JSON file to read.

    Returns:
        The decoded JSON object.

    Raises:
        ValueError: If the file does not contain a JSON object.
        OSError: If the file cannot be read.
    """

    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object in {path}")
    return value


def parse_versions(path: Path) -> dict[str, str]:
    """Parse simple ``KEY=value`` entries from a versions file.

    Args:
        path: Environment-style versions file.

    Returns:
        A mapping of variable names to values.

    Raises:
        ValueError: If a non-comment line has no ``=`` separator.
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


def validate_scenario_config(config: dict[str, Any]) -> None:
    """Validate the parts of the scenario contract used by the runner.

    Args:
        config: Decoded scenario configuration.

    Returns:
        None.

    Raises:
        ValueError: If a required field, load point, or profile is invalid.
    """

    defaults = config.get("defaults")
    scenarios = config.get("scenarios")
    profiles = config.get("profiles")
    if not isinstance(defaults, dict) or not isinstance(scenarios, list) or not isinstance(profiles, dict):
        raise ValueError("scenario config requires defaults, profiles, and scenarios")
    for name in ("warmup_seconds", "duration_seconds", "repetitions", "connections"):
        if not isinstance(defaults.get(name), (int, float)) or defaults[name] <= 0:
            raise ValueError(f"defaults.{name} must be positive")
    stable_error_rate = defaults.get("stable_error_rate")
    if not isinstance(stable_error_rate, (int, float)) or not 0 <= stable_error_rate < 1:
        raise ValueError("defaults.stable_error_rate must be in [0, 1)")

    scenario_ids: set[str] = set()
    for scenario in scenarios:
        if not isinstance(scenario, dict):
            raise ValueError("each scenario must be an object")
        required = ("id", "title", "scheme", "protocol", "path", "load_model", "reference_load")
        missing = [field for field in required if field not in scenario]
        if missing:
            raise ValueError(f"scenario is missing fields: {', '.join(missing)}")
        scenario_id = scenario["id"]
        if not isinstance(scenario_id, str) or not scenario_id or scenario_id in scenario_ids:
            raise ValueError(f"scenario id must be unique and non-empty: {scenario_id!r}")
        scenario_ids.add(scenario_id)
        if scenario["scheme"] not in ("http", "https"):
            raise ValueError(f"unsupported scheme for {scenario_id}")
        if scenario["protocol"] not in ("http1", "http2"):
            raise ValueError(f"unsupported protocol for {scenario_id}")
        http2_parallel = scenario.get("http2_parallel", 1)
        if not isinstance(http2_parallel, int) or http2_parallel <= 0:
            raise ValueError(f"{scenario_id}.http2_parallel must be a positive integer")
        if scenario["load_model"] not in ("qps", "concurrency"):
            raise ValueError(f"unsupported load model for {scenario_id}")
        points_key = f"{scenario['load_model']}_points"
        points = scenario.get(points_key)
        if not isinstance(points, list) or not points or any(not isinstance(point, int) or point <= 0 for point in points):
            raise ValueError(f"{scenario_id}.{points_key} must contain positive integers")
        if scenario["reference_load"] not in points:
            raise ValueError(f"{scenario_id}.reference_load must be one of {points}")
        flags = scenario.get("flags", [])
        if not isinstance(flags, list) or any(not isinstance(flag, str) for flag in flags):
            raise ValueError(f"{scenario_id}.flags must be a string list")

    for profile_name, profile in profiles.items():
        if not isinstance(profile, dict):
            raise ValueError(f"profile {profile_name} must be an object")
        ids = profile.get("scenario_ids", "all")
        if ids != "all" and (not isinstance(ids, list) or any(item not in scenario_ids for item in ids)):
            raise ValueError(f"profile {profile_name} contains an unknown scenario")
        overrides = profile.get("load_points", {})
        if not isinstance(overrides, dict):
            raise ValueError(f"profile {profile_name}.load_points must be an object")
        for scenario_id, points in overrides.items():
            if scenario_id not in scenario_ids or not isinstance(points, list) or not points:
                raise ValueError(f"invalid load_points override for {scenario_id}")
            if any(not isinstance(point, int) or point <= 0 for point in points):
                raise ValueError(f"load_points for {scenario_id} must be positive integers")


def iter_plan(config: dict[str, Any], profile_name: str) -> list[dict[str, Any]]:
    """Expand one named profile into executable load points.

    Args:
        config: Validated scenario configuration.
        profile_name: Profile to expand, such as ``quick`` or ``full``.

    Returns:
        One plan record per scenario and load point.

    Raises:
        ValueError: If the profile does not exist or an override is invalid.
    """

    validate_scenario_config(config)
    defaults = config["defaults"]
    profile = config.get("profiles", {}).get(profile_name)
    if not isinstance(profile, dict):
        raise ValueError(f"unknown benchmark profile: {profile_name}")
    selected_ids = profile.get("scenario_ids", "all")
    selected = set(item["id"] for item in config["scenarios"] if selected_ids == "all" or item["id"] in selected_ids)
    plans: list[dict[str, Any]] = []
    overrides = profile.get("load_points", {})
    for scenario in config["scenarios"]:
        if scenario["id"] not in selected:
            continue
        load_key = f"{scenario['load_model']}_points"
        points = overrides.get(scenario["id"], scenario[load_key])
        if not points:
            raise ValueError(f"profile {profile_name} has no load points for {scenario['id']}")
        reference_load = profile.get("reference_loads", {}).get(
            scenario["id"], scenario["reference_load"]
        )
        if reference_load not in points:
            reference_load = points[len(points) // 2]
        for load in points:
            plans.append(
                {
                    "scenario_id": scenario["id"],
                    "title": scenario["title"],
                    "scheme": scenario["scheme"],
                    "protocol": scenario["protocol"],
                    "path": scenario["path"],
                    "load_model": scenario["load_model"],
                    "load": load,
                    "connections": scenario.get("connections", defaults["connections"]),
                    "http2_parallel": scenario.get("http2_parallel", 1),
                    "warmup_seconds": profile.get("warmup_seconds", scenario.get("warmup_seconds", defaults["warmup_seconds"])),
                    "duration_seconds": profile.get("duration_seconds", scenario.get("duration_seconds", defaults["duration_seconds"])),
                    "repetitions": profile.get("repetitions", scenario.get("repetitions", defaults["repetitions"])),
                    "reference_load": reference_load,
                    "stable_error_rate": defaults["stable_error_rate"],
                    "flags": list(scenario.get("flags", [])),
                }
            )
    if not plans:
        raise ValueError(f"profile {profile_name} selected no scenarios")
    return plans


def print_plan_shell(config: dict[str, Any], profile_name: str) -> None:
    """Print an unambiguous pipe-delimited plan for the shell runner.

    Args:
        config: Validated scenario configuration.
        profile_name: Profile to expand.

    Returns:
        None. One line is written for each executable load point.

    Raises:
        ValueError: If a field contains the reserved pipe delimiter.
    """

    fields = (
        "scenario_id",
        "scheme",
        "protocol",
        "path",
        "load_model",
        "load",
        "connections",
        "http2_parallel",
        "warmup_seconds",
        "duration_seconds",
        "repetitions",
        "flags",
        "reference_load",
    )
    for plan in iter_plan(config, profile_name):
        values = [
            ",".join(plan["flags"]) if field == "flags" else str(plan[field])
            for field in fields
        ]
        if any("|" in value for value in values):
            raise ValueError("scenario values must not contain '|'")
        print("|".join(values))


def parse_memory_bytes(value: str) -> float:
    """Parse a Docker memory quantity such as ``12.5MiB`` into bytes.

    Args:
        value: Human-readable Docker memory quantity.

    Returns:
        Memory size in bytes.

    Raises:
        ValueError: If the value is not a recognized quantity.
    """

    match = re.fullmatch(r"\s*([0-9]+(?:\.[0-9]+)?)\s*([A-Za-z]+)\s*", value)
    if not match:
        raise ValueError(f"invalid memory quantity: {value!r}")
    number, unit = match.groups()
    multiplier = MEMORY_UNITS.get(unit.upper())
    if multiplier is None:
        raise ValueError(f"unknown memory unit: {unit}")
    return float(number) * multiplier


def parse_cpu_percent(value: str) -> float:
    """Parse a Docker CPU percentage.

    Args:
        value: Value such as ``12.34%``.

    Returns:
        CPU percentage as a floating-point number.

    Raises:
        ValueError: If the value is not numeric.
    """

    return float(value.strip().rstrip("%"))


def read_stats(path: Path) -> dict[str, float]:
    """Aggregate sampled Docker CPU and memory statistics.

    Args:
        path: Tab-separated stats file written by ``run.sh``.

    Returns:
        Average and peak CPU percentages plus peak memory bytes.

    Raises:
        ValueError: If the file contains no valid samples.
        OSError: If the file cannot be read.
    """

    cpu_values: list[float] = []
    memory_values: list[float] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("timestamp\t"):
            continue
        fields = line.split("\t", 2)
        if len(fields) != 3:
            continue
        try:
            cpu_values.append(parse_cpu_percent(fields[1]))
            memory_values.append(parse_memory_bytes(fields[2].split("/", 1)[0]))
        except ValueError:
            continue
    if not cpu_values or not memory_values:
        raise ValueError(f"no valid Docker stats samples in {path}")
    return {
        "cpu_avg_percent": statistics.fmean(cpu_values),
        "cpu_peak_percent": max(cpu_values),
        "memory_peak_bytes": max(memory_values),
    }


def _normalise_success_rate(value: Any) -> float:
    rate = float(value)
    if rate > 1:
        rate /= 100
    if not 0 <= rate <= 1:
        raise ValueError(f"success rate is outside [0, 1]: {value!r}")
    return rate


def parse_oha(path: Path) -> dict[str, float]:
    """Extract stable metrics from an oha JSON result.

    Args:
        path: JSON output produced by oha with ``--output-format json``.

    Returns:
        QPS, success/error rates, and latency percentiles in milliseconds.

    Raises:
        ValueError: If required oha fields are missing or invalid.
        OSError: If the file cannot be read.
    """

    payload = load_json(path)
    metrics = payload.get("metrics")
    latency = metrics.get("latency_ms") if isinstance(metrics, dict) else None
    if not isinstance(metrics, dict) or not isinstance(latency, dict):
        raise ValueError(f"missing metrics in oha result {path}")
    required = ("requests_per_sec", "success_rate")
    if any(field not in metrics for field in required):
        raise ValueError(f"missing required metrics in oha result {path}")
    latency_fields = {"p50": "p50_ms", "p95": "p95_ms", "p99": "p99_ms", "max": "max_ms"}
    if any(field not in latency for field in latency_fields):
        raise ValueError(f"missing latency metrics in oha result {path}")
    success_rate = _normalise_success_rate(metrics["success_rate"])
    return {
        "qps": float(metrics["requests_per_sec"]),
        "success_rate": success_rate,
        "error_rate": 1.0 - success_rate,
        **{output: float(latency[input_name]) for input_name, output in latency_fields.items()},
    }


def median(values: Iterable[float]) -> float:
    """Return the median of a non-empty numeric iterable.

    Args:
        values: Numeric values to reduce.

    Returns:
        The statistical median.

    Raises:
        ValueError: If ``values`` is empty.
    """

    values_list = list(values)
    if not values_list:
        raise ValueError("cannot take the median of an empty sequence")
    return float(statistics.median(values_list))


def aggregate_measurements(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Aggregate repeated raw records by scenario, target, and load point.

    Args:
        records: One parsed record per target/repetition/load point.

    Returns:
        Median measurements with derived CPU cost per request.

    Raises:
        ValueError: If records are empty or a measurement has invalid QPS.
    """

    if not records:
        raise ValueError("no raw measurements were found")
    grouped: dict[tuple[str, str, int], list[dict[str, Any]]] = {}
    for record in records:
        key = (record["scenario_id"], record["target"], int(record["load"]))
        grouped.setdefault(key, []).append(record)

    measurements: list[dict[str, Any]] = []
    for group in grouped.values():
        first = group[0]
        metrics = {
            name: median(record["metrics"][name] for record in group)
            for name in NUMERIC_METRICS
            if name != "cpu_ms_per_request"
        }
        if metrics["qps"] <= 0:
            raise ValueError(f"non-positive QPS for {first['scenario_id']} / {first['target']}")
        metrics["cpu_ms_per_request"] = (
            metrics["cpu_avg_percent"] / 100.0 * 1000.0 / metrics["qps"]
        )
        measurements.append(
            {
                "scenario_id": first["scenario_id"],
                "title": first["title"],
                "scheme": first["scheme"],
                "protocol": first["protocol"],
                "path": first["path"],
                "load_model": first["load_model"],
                "load": first["load"],
                "reference_load": first["reference_load"],
                "target": first["target"],
                "replicates": len(group),
                "metrics": metrics,
            }
        )
    return sorted(measurements, key=lambda item: (item["scenario_id"], item["target"], item["load"]))


def select_primary_measurements(
    measurements: list[dict[str, Any]], stable_error_rate: float
) -> list[dict[str, Any]]:
    """Select reference-load and maximum-stable-throughput metrics.

    Args:
        measurements: Median measurements from ``aggregate_measurements``.
        stable_error_rate: Maximum allowed error rate for stable throughput.

    Returns:
        One primary record per scenario and target.

    Raises:
        ValueError: If a target or reference load is missing, or no stable point exists.
    """

    grouped: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for measurement in measurements:
        grouped.setdefault((measurement["scenario_id"], measurement["target"]), []).append(measurement)
    primary: list[dict[str, Any]] = []
    for (scenario_id, target), points in grouped.items():
        reference_load = points[0]["reference_load"]
        reference = next((point for point in points if point["load"] == reference_load), None)
        if reference is None:
            raise ValueError(f"missing reference load {reference_load} for {scenario_id} / {target}")
        stable = [point for point in points if point["metrics"]["error_rate"] <= stable_error_rate]
        if not stable:
            raise ValueError(f"no stable load point for {scenario_id} / {target}")
        best = max(stable, key=lambda point: point["metrics"]["qps"])
        primary.append(
            {
                "scenario_id": scenario_id,
                "title": reference["title"],
                "scheme": reference["scheme"],
                "protocol": reference["protocol"],
                "path": reference["path"],
                "load_model": reference["load_model"],
                "target": target,
                "reference_load": reference_load,
                "reference_metrics": reference["metrics"],
                "max_stable_load": best["load"],
                "max_stable_qps": best["metrics"]["qps"],
            }
        )
    return sorted(primary, key=lambda item: (item["scenario_id"], TARGETS.index(item["target"])))


def normalize_percentage(value: float, baseline: float, metric_name: str) -> float:
    """Normalize a metric to a baseline of 100 percent.

    Args:
        value: Target metric value.
        baseline: Nginx metric value for the same scenario and load.
        metric_name: Name used in diagnostic errors.

    Returns:
        ``value / baseline * 100``.

    Raises:
        ValueError: If either value is not finite or the baseline is not positive.
    """

    if not math.isfinite(value) or not math.isfinite(baseline) or baseline <= 0:
        raise ValueError(f"cannot normalize {metric_name}: invalid baseline or value")
    return value / baseline * 100.0


def build_normalized(primary: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Build Nginx-relative metrics for each primary scenario result.

    Args:
        primary: Primary records from ``select_primary_measurements``.

    Returns:
        One record per scenario and target with percentage metrics.

    Raises:
        ValueError: If a scenario lacks an Nginx baseline or required metric.
    """

    grouped: dict[str, dict[str, dict[str, Any]]] = {}
    for item in primary:
        grouped.setdefault(item["scenario_id"], {})[item["target"]] = item
    normalized: list[dict[str, Any]] = []
    for scenario_id, targets in grouped.items():
        baseline = targets.get("nginx")
        if baseline is None:
            raise ValueError(f"missing nginx baseline for {scenario_id}")
        base_ref = baseline["reference_metrics"]
        metrics_by_target: dict[str, dict[str, float]] = {}
        for target in TARGETS:
            item = targets.get(target)
            if item is None:
                raise ValueError(f"missing {target} result for {scenario_id}")
            reference = item["reference_metrics"]
            metrics_by_target[target] = {
                "throughput_pct": 100.0
                if target == "nginx"
                else normalize_percentage(item["max_stable_qps"], baseline["max_stable_qps"], "throughput"),
                "p99_pct": 100.0
                if target == "nginx"
                else normalize_percentage(reference["p99_ms"], base_ref["p99_ms"], "p99 latency"),
                "cpu_per_request_pct": 100.0
                if target == "nginx"
                else normalize_percentage(
                    reference["cpu_ms_per_request"], base_ref["cpu_ms_per_request"], "CPU per request"
                ),
                "memory_pct": 100.0
                if target == "nginx"
                else normalize_percentage(
                    reference["memory_peak_bytes"], base_ref["memory_peak_bytes"], "memory"
                ),
                "error_rate": reference["error_rate"],
            }
        for target in TARGETS:
            item = targets[target]
            normalized.append(
                {
                    "scenario_id": scenario_id,
                    "title": item["title"],
                    "scheme": item["scheme"],
                    "protocol": item["protocol"],
                    "path": item["path"],
                    "load_model": item["load_model"],
                    "target": target,
                    "reference_load": item["reference_load"],
                    "max_stable_load": item["max_stable_load"],
                    "max_stable_qps": item["max_stable_qps"],
                    "reference_metrics": item["reference_metrics"],
                    "metrics": metrics_by_target[target],
                }
            )
    return normalized


def build_manifest(
    bench_root: Path,
    run_id: str,
    profile_name: str,
    raddex_commit: str,
    raddex_threads: int,
) -> dict[str, Any]:
    """Build a reproducibility manifest without host-identifying fields.

    Args:
        bench_root: Mounted benchmark directory.
        run_id: Unique run identifier.
        profile_name: Selected benchmark profile.
        raddex_commit: Raddex commit used by the image build.
        raddex_threads: Pingora worker threads used by the Raddex target.

    Returns:
        Manifest containing tool versions and configuration hashes.

    Raises:
        OSError: If a required manifest input cannot be read.
    """

    config_hashes: dict[str, str] = {}
    for target in TARGETS:
        config_dir = bench_root / "configs" / target
        for path in sorted(config_dir.iterdir()):
            if path.is_file():
                config_hashes[f"{target}/{path.name}"] = sha256_file(path)
    return {
        "schema_version": 1,
        "run_id": run_id,
        "profile": profile_name,
        "raddex_commit": raddex_commit,
        "raddex_threads": raddex_threads,
        "tools": parse_versions(bench_root / "versions.env"),
        "config_sha256": config_hashes,
    }


def collect_results(
    bench_root: Path, raw_dir: Path, scenario_file: Path, profile_name: str, manifest: dict[str, Any]
) -> dict[str, Any]:
    """Read raw files and produce the normalized summary object.

    Args:
        bench_root: Mounted benchmark directory.
        raw_dir: Directory containing per-target raw results.
        scenario_file: Scenario configuration path.
        profile_name: Profile used to produce the raw results.
        manifest: Run manifest to embed in the summary.

    Returns:
        Summary object consumed by the report generator.

    Raises:
        ValueError: If raw results are incomplete or invalid.
        OSError: If a result file cannot be read.
    """

    config = load_json(scenario_file)
    plans = iter_plan(config, profile_name)
    records: list[dict[str, Any]] = []
    for plan in plans:
        for target in TARGETS:
            point_dir = raw_dir / plan["scenario_id"] / target / f"{plan['load_model']}-{plan['load']}"
            for repetition in range(1, int(plan["repetitions"]) + 1):
                result_path = point_dir / f"rep-{repetition}.json"
                stats_path = point_dir / f"rep-{repetition}.stats.tsv"
                meta_path = point_dir / f"rep-{repetition}.meta.json"
                if not result_path.is_file() or not stats_path.is_file() or not meta_path.is_file():
                    raise ValueError(f"incomplete raw result for {plan['scenario_id']} / {target} / rep-{repetition}")
                meta = load_json(meta_path)
                if int(meta.get("exit_code", 1)) != 0:
                    raise ValueError(f"load generator failed for {plan['scenario_id']} / {target} / rep-{repetition}")
                oha_metrics = parse_oha(result_path)
                stats_metrics = read_stats(stats_path)
                records.append(
                    {
                        **plan,
                        "target": target,
                        "repetition": repetition,
                        "metrics": {**oha_metrics, **stats_metrics},
                    }
                )
    measurements = aggregate_measurements(records)
    primary = select_primary_measurements(measurements, float(config["defaults"]["stable_error_rate"]))
    normalized = build_normalized(primary)
    return {
        "schema_version": 1,
        "manifest": manifest,
        "scenario_file": str(scenario_file),
        "measurements": measurements,
        "primary": primary,
        "normalized": normalized,
    }


def parse_args() -> argparse.Namespace:
    """Parse command-line arguments for planning, manifest, or collection."""

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario-file", type=Path, default=Path("/bench/scenarios/scenarios.json"))
    parser.add_argument("--profile", default="quick")
    parser.add_argument("--plan-shell", action="store_true")
    parser.add_argument("--write-manifest", action="store_true")
    parser.add_argument("--collect", action="store_true")
    parser.add_argument("--bench-root", type=Path, default=Path("/bench"))
    parser.add_argument("--raw-dir", type=Path)
    parser.add_argument("--run-id")
    parser.add_argument("--raddex-commit", default="unknown")
    parser.add_argument("--raddex-threads", type=int, default=1)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--summary-out", type=Path)
    return parser.parse_args()


def main() -> int:
    """Run the requested planning or collection operation."""

    args = parse_args()
    try:
        config = load_json(args.scenario_file)
        if args.plan_shell:
            print_plan_shell(config, args.profile)
            return 0
        if args.write_manifest:
            if not args.run_id:
                raise ValueError("--run-id is required with --write-manifest")
            if args.raddex_threads < 1:
                raise ValueError("--raddex-threads must be at least 1")
            manifest = build_manifest(
                args.bench_root,
                args.run_id,
                args.profile,
                args.raddex_commit,
                args.raddex_threads,
            )
            output = args.bench_root / "results" / args.run_id / "run.json"
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            return 0
        if args.collect:
            if args.raw_dir is None or args.manifest is None or args.summary_out is None:
                raise ValueError("--raw-dir, --manifest, and --summary-out are required with --collect")
            manifest = load_json(args.manifest)
            summary = collect_results(args.bench_root, args.raw_dir, args.scenario_file, args.profile, manifest)
            args.summary_out.parent.mkdir(parents=True, exist_ok=True)
            args.summary_out.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            return 0
        raise ValueError("choose one of --plan-shell, --write-manifest, or --collect")
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"collect error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
