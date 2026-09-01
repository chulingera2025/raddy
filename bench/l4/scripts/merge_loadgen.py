#!/usr/bin/env python3
"""Merge parallel L4 load-generator JSON results."""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path
from typing import Any

from collect import NUMERIC_FIELDS


SUM_FIELDS = (
    "requested_connections",
    "successful_connections",
    "failed_connections",
    "completed_operations",
    "sent_bytes",
    "received_bytes",
    "sent_packets",
    "received_packets",
    "errors",
)
MAX_FIELDS = (
    "elapsed_seconds",
    "establishment_seconds",
    "max_latency_us",
    "max_connect_latency_us",
)
UNMERGEABLE_PERCENTILES = (
    "p50_latency_us",
    "p95_latency_us",
    "p99_latency_us",
    "p50_connect_latency_us",
    "p95_connect_latency_us",
    "p99_connect_latency_us",
)


def load(path: Path) -> dict[str, Any]:
    """Load one shard result.

    Args:
        path: JSON result path.

    Returns:
        Decoded result object.

    Raises:
        ValueError: If the result has an unsupported schema.
        OSError: If the file cannot be read.
    """

    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise ValueError(f"invalid loadgen shard: {path}")
    if not isinstance(value.get("mode"), str) or not value["mode"]:
        raise ValueError(f"loadgen shard has no mode: {path}")
    for field in NUMERIC_FIELDS:
        if field not in value:
            raise ValueError(f"missing loadgen field {field} in {path}")
        if value[field] is None:
            continue
        try:
            number = float(value[field])
        except (TypeError, ValueError) as error:
            raise ValueError(f"invalid loadgen field {field} in {path}") from error
        if not math.isfinite(number):
            raise ValueError(f"non-finite loadgen field {field} in {path}")
    return value


def merge(paths: list[Path]) -> dict[str, Any]:
    """Merge shard counters and derive aggregate rates.

    Args:
        paths: Non-empty shard result paths.

    Returns:
        A schema-compatible aggregate result.

    Raises:
        ValueError: If shard data is incomplete, modes differ, or no paths are supplied.
        OSError: If a shard cannot be read.
    """

    if not paths:
        raise ValueError("at least one shard is required")
    shards = [load(path) for path in paths]
    mode = shards[0].get("mode")
    if any(shard.get("mode") != mode for shard in shards):
        raise ValueError("loadgen shard modes differ")
    output = dict(shards[0])
    for field in SUM_FIELDS:
        output[field] = sum(int(shard.get(field, 0)) for shard in shards)
    for field in MAX_FIELDS:
        values = [shard.get(field) for shard in shards if shard.get(field) is not None]
        output[field] = max(values) if values else None
    for field in UNMERGEABLE_PERCENTILES:
        output[field] = None
    elapsed = max(float(shard.get("elapsed_seconds", 0.0)) for shard in shards)
    elapsed = max(elapsed, 0.001)
    establishment = max(float(shard.get("establishment_seconds", 0.0)) for shard in shards)
    establishment = max(establishment, 0.001)
    output["elapsed_seconds"] = elapsed
    output["establishment_seconds"] = establishment
    output["success_rate"] = (
        output["received_packets"] / output["sent_packets"]
        if output["sent_packets"] > 0
        else output["successful_connections"] / output["requested_connections"]
        if output["requested_connections"] > 0
        else 0.0
    )
    output["success_rate"] = max(0.0, min(1.0, float(output["success_rate"])))
    output["error_rate"] = 1.0 - output["success_rate"]
    output["packet_loss_pct"] = (
        (1.0 - output["received_packets"] / output["sent_packets"]) * 100.0
        if output["sent_packets"] > 0
        else 0.0
    )
    output["throughput_mbps"] = output["received_bytes"] * 8.0 / elapsed / 1_000_000.0
    output["packets_per_second"] = output["received_packets"] / elapsed
    output["connection_rate_per_second"] = output["successful_connections"] / establishment
    return output


def parse_args() -> argparse.Namespace:
    """Parse merge command-line arguments.

    Returns:
        Parsed command-line namespace.

    Raises:
        SystemExit: If argparse rejects the command line.
    """

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("inputs", type=Path, nargs="+")
    return parser.parse_args()


def main() -> int:
    """Merge shard results and return a shell-friendly status code.

    Returns:
        Zero on success, otherwise one after printing a diagnostic.

    Raises:
        None: Expected input errors are converted into a non-zero status.
    """

    args = parse_args()
    try:
        result = merge(args.inputs)
        args.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        return 0
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"merge error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
