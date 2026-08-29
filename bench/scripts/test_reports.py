#!/usr/bin/env python3
"""Unit tests for benchmark planning, aggregation, and normalization."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from collect import (
    aggregate_measurements,
    build_normalized,
    median,
    normalize_percentage,
    parse_memory_bytes,
    parse_oha,
    read_stats,
    select_primary_measurements,
    validate_scenario_config,
)


def sample_metrics(qps: float, p99_ms: float, cpu_ms: float, memory: float) -> dict[str, float]:
    """Build a complete synthetic metrics record for unit tests."""

    return {
        "qps": qps,
        "success_rate": 1.0,
        "error_rate": 0.0,
        "p50_ms": p99_ms / 2,
        "p95_ms": p99_ms * 0.8,
        "p99_ms": p99_ms,
        "max_ms": p99_ms * 1.1,
        "cpu_avg_percent": cpu_ms * qps / 10,
        "cpu_peak_percent": cpu_ms * qps / 8,
        "memory_peak_bytes": memory,
        "cpu_ms_per_request": cpu_ms,
    }


class CollectionTests(unittest.TestCase):
    """Verify the pure collection and normalization rules."""

    def test_median_and_memory_parsing(self) -> None:
        self.assertEqual(median([3, 1, 2]), 2.0)
        self.assertEqual(parse_memory_bytes("2MiB"), 2 * 1024 * 1024)
        self.assertEqual(parse_memory_bytes("1GB"), 1000**3)

    def test_normalization_uses_the_same_scenario_baseline(self) -> None:
        self.assertEqual(normalize_percentage(125, 100, "throughput"), 125.0)
        with self.assertRaises(ValueError):
            normalize_percentage(1, 0, "throughput")

    def test_normalization_rejects_missing_target_baseline(self) -> None:
        primary = [
            {
                "scenario_id": "sample",
                "title": "Sample",
                "target": "caddy",
                "reference_load": 1,
                "reference_metrics": sample_metrics(1, 1, 1, 1),
                "max_stable_load": 1,
                "max_stable_qps": 1,
            }
        ]
        with self.assertRaises(ValueError):
            build_normalized(primary)

    def test_primary_and_normalized_results_keep_nginx_at_100(self) -> None:
        records = []
        for target, qps, p99, cpu, memory in (
            ("nginx", 1000, 2, 1, 100),
            ("caddy", 900, 3, 2, 120),
            ("raddy", 1100, 1.5, 0.8, 90),
        ):
            for repetition in (1, 2, 3):
                records.append(
                    {
                        "scenario_id": "sample",
                        "title": "Sample",
                        "scheme": "http",
                        "protocol": "http1",
                        "path": "/small",
                        "load_model": "qps",
                        "load": 1000,
                        "reference_load": 1000,
                        "target": target,
                        "metrics": sample_metrics(qps, p99, cpu, memory),
                    }
                )
        measurements = aggregate_measurements(records)
        primary = select_primary_measurements(measurements, 0.001)
        normalized = build_normalized(primary)
        nginx = next(item for item in normalized if item["target"] == "nginx")
        caddy = next(item for item in normalized if item["target"] == "caddy")
        self.assertEqual(nginx["metrics"]["throughput_pct"], 100.0)
        self.assertEqual(nginx["metrics"]["p99_pct"], 100.0)
        self.assertAlmostEqual(caddy["metrics"]["throughput_pct"], 90.0)
        self.assertAlmostEqual(caddy["metrics"]["p99_pct"], 150.0)

    def test_scenario_validation_rejects_bad_reference(self) -> None:
        config = {
            "defaults": {
                "warmup_seconds": 1,
                "duration_seconds": 1,
                "repetitions": 1,
                "connections": 1,
                "stable_error_rate": 0.001,
            },
            "profiles": {"quick": {"scenario_ids": "all"}},
            "scenarios": [
                {
                    "id": "bad",
                    "title": "Bad",
                    "scheme": "http",
                    "protocol": "http1",
                    "path": "/",
                    "load_model": "qps",
                    "qps_points": [10],
                    "reference_load": 20,
                }
            ],
        }
        with self.assertRaises(ValueError):
            validate_scenario_config(config)

    def test_oha_and_stats_files_are_parsed(self) -> None:
        oha_payload = {
            "metrics": {
                "success_rate": 1.0,
                "requests_per_sec": 100.0,
                "latency_ms": {"p50": 1, "p95": 2, "p99": 3, "max": 4},
            }
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            oha_path = root / "result.json"
            stats_path = root / "stats.tsv"
            oha_path.write_text(json.dumps(oha_payload), encoding="utf-8")
            stats_path.write_text(
                "timestamp\tcpu_percent\tmemory_usage\n"
                "1\t10.0%\t2MiB / 1GiB\n"
                "2\t20.0%\t3MiB / 1GiB\n",
                encoding="utf-8",
            )
            self.assertEqual(parse_oha(oha_path)["p99_ms"], 3.0)
            self.assertEqual(read_stats(stats_path)["memory_peak_bytes"], 3 * 1024 * 1024)


if __name__ == "__main__":
    unittest.main()
