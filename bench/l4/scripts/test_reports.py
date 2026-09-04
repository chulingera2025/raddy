#!/usr/bin/env python3
"""Unit tests for L4 scenario validation, normalization, and reporting."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from collect import (
    aggregate_measurements,
    normalize_measurements,
    parse_host_delta,
    parse_stats,
    validate_config,
)
from merge_loadgen import merge
from report import _normalized_table, create_overview_chart, load_summary


def raw_metrics(multiplier: float = 1.0) -> dict[str, float]:
    """Build a complete raw metric object for normalization tests.

    Args:
        multiplier: Factor applied to each positive metric.

    Returns:
        A complete raw metric mapping.

    Raises:
        None.
    """

    return {
        "throughput_mbps": 100.0 * multiplier,
        "packets_per_second": 1000.0 * multiplier,
        "connection_rate_per_second": 100.0 * multiplier,
        "successful_connections": 100.0 * multiplier,
        "p99_latency_ms": 1.0 * multiplier,
        "cpu_ms_per_operation": 2.0 * multiplier,
        "cpu_ms_per_connection": 3.0 * multiplier,
        "peak_memory_bytes": 4_000.0 * multiplier,
        "error_rate": 0.0,
        "packet_loss_pct": 0.0,
    }


class L4ReportTests(unittest.TestCase):
    """Verify the pure L4 report and normalization contracts."""

    def test_scenario_config_is_valid(self) -> None:
        """Ensure the checked-in quick and full profiles are valid.

        Returns:
            None.

        Raises:
            AssertionError: If the scenario configuration is invalid.
        """

        scenario_path = Path(__file__).resolve().parents[1] / "scenarios" / "scenarios.json"
        config = json.loads(scenario_path.read_text(encoding="utf-8"))
        validate_config(config)

    def test_normalization_keeps_nginx_at_one_hundred_percent(self) -> None:
        """Ensure each scenario uses its own Nginx baseline.

        Returns:
            None.

        Raises:
            AssertionError: If normalized values violate the baseline contract.
        """

        measurements = []
        for target, multiplier in (
            ("nginx", 1.0),
            ("caddy", 0.5),
            ("raddex-l4", 0.7),
            ("nat", 0.25),
        ):
            measurements.append(
                {
                    "scenario_id": "tcp-throughput",
                    "title": "TCP throughput / sample",
                    "kind": "tcp_throughput",
                    "mode": "tcp-throughput",
                    "transport": "tcp",
                    "target": target,
                    "connections": 16,
                    "payload_bytes": 1024,
                    "metrics": raw_metrics(multiplier),
                }
            )
        normalized = normalize_measurements(measurements)
        nginx = next(item for item in normalized if item["target"] == "nginx")
        caddy = next(item for item in normalized if item["target"] == "caddy")
        nat = next(item for item in normalized if item["target"] == "nat")
        self.assertEqual(nginx["metrics"]["throughput_pct"], 100.0)
        self.assertEqual(nginx["metrics"]["memory_pct"], 100.0)
        self.assertAlmostEqual(caddy["metrics"]["throughput_pct"], 50.0)
        self.assertAlmostEqual(caddy["metrics"]["cpu_pct"], 50.0)
        self.assertIsNone(nat["metrics"]["cpu_pct"])
        self.assertIsNone(nat["metrics"]["memory_pct"])

    def test_normalization_rejects_a_zero_nginx_baseline(self) -> None:
        """Ensure zero Nginx metrics fail instead of producing an invalid ratio.

        Returns:
            None.

        Raises:
            AssertionError: If a zero baseline is accepted.
        """

        measurements = []
        for target in ("nginx", "caddy", "raddex-l4", "nat"):
            metrics = raw_metrics()
            if target == "nginx":
                metrics["throughput_mbps"] = 0.0
            measurements.append(
                {
                    "scenario_id": "tcp-throughput",
                    "title": "TCP throughput / sample",
                    "kind": "tcp_throughput",
                    "mode": "tcp-throughput",
                    "transport": "tcp",
                    "target": target,
                    "connections": 16,
                    "payload_bytes": 1024,
                    "metrics": metrics,
                }
            )
        with self.assertRaises(ValueError):
            normalize_measurements(measurements)

    def test_median_aggregation_uses_the_middle_repetition(self) -> None:
        """Ensure repeated raw measurements are aggregated by median.

        Returns:
            None.

        Raises:
            AssertionError: If the aggregate is not the median repetition.
        """

        records = []
        for throughput in (10.0, 30.0, 20.0):
            records.append(
                {
                    "scenario_id": "tcp-throughput",
                    "title": "TCP throughput / sample",
                    "kind": "tcp_throughput",
                    "mode": "tcp-throughput",
                    "transport": "tcp",
                    "target": "nginx",
                    "connections": 1,
                    "payload_bytes": 1024,
                    "metrics": {
                        "throughput_mbps": throughput,
                        "cpu_basis": "target_cgroup",
                    },
                }
            )
        result = aggregate_measurements(records)

        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["replicates"], 3)
        self.assertEqual(result[0]["metrics"]["throughput_mbps"], 20.0)

    def test_host_cpu_delta_rejects_incomplete_snapshots(self) -> None:
        """Ensure host CPU collection fails clearly when rows are missing.

        Returns:
            None.

        Raises:
            AssertionError: If incomplete snapshots are accepted.
        """

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "host.tsv"
            path.write_text("1\t10\t5\t2\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                parse_host_delta(path)

    def test_report_rejects_a_summary_with_missing_targets(self) -> None:
        """Ensure malformed normalized reports fail with a clear diagnostic.

        Returns:
            None.

        Raises:
            AssertionError: If an incomplete scenario is accepted.
        """

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "summary.json"
            path.write_text(
                json.dumps(
                    {
                        "normalized": [
                            {"scenario_id": "sample", "target": "nginx", "metrics": {}}
                        ]
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaises(ValueError):
                load_summary(path)

    def test_nat_stats_require_conntrack_and_slab_samples(self) -> None:
        """Ensure missing kernel state is not reported as a zero measurement.

        Returns:
            None.

        Raises:
            AssertionError: If missing NAT state is silently converted to zero.
        """

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "stats.tsv"
            path.write_text(
                "timestamp\tcpu_percent\tmemory_usage\tconntrack_count\tnf_conntrack_objects\tnf_conntrack_bytes\n"
                "1\t1%\t1MiB / 1GiB\t\t\t\n",
                encoding="utf-8",
            )
            with self.assertRaises(ValueError):
                parse_stats(path, require_nat_state=True)

    def test_nat_stats_capture_approximate_slab_bytes(self) -> None:
        """Ensure active conntrack slab bytes are parsed when available.

        Returns:
            None.

        Raises:
            AssertionError: If valid NAT slab samples are parsed incorrectly.
        """

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "stats.tsv"
            path.write_text(
                "timestamp\tcpu_percent\tmemory_usage\tconntrack_count\tnf_conntrack_objects\tnf_conntrack_bytes\n"
                "1\t1%\t1MiB / 1GiB\t5\t100\t32000\n",
                encoding="utf-8",
            )
            metrics = parse_stats(path, require_nat_state=True)
            self.assertEqual(metrics["conntrack_peak"], 5.0)
            self.assertEqual(metrics["nf_conntrack_objects_peak"], 100.0)
            self.assertEqual(metrics["nf_conntrack_bytes_peak"], 32000.0)

    def test_cgroup_stats_capture_memory_and_task_fields(self) -> None:
        """Ensure optional cgroup v2 samples are retained in parsed stats.

        Returns:
            None.

        Raises:
            AssertionError: If cgroup samples are lost or parsed incorrectly.
        """

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "stats.tsv"
            path.write_text(
                "timestamp\tcpu_percent\tmemory_usage\tconntrack_count\tnf_conntrack_objects\tnf_conntrack_bytes\tmemory_current\tmemory_peak_cgroup\tmemory_anon\tmemory_file\tmemory_kernel\tmemory_sock\tpids_current\tthreads_current\n"
                "1\t1%\t1MiB / 1GiB\t\t\t\t100\t200\t300\t400\t50\t6\t2\t8\n",
                encoding="utf-8",
            )
            metrics = parse_stats(path)
            self.assertEqual(metrics["memory_current_peak"], 100.0)
            self.assertEqual(metrics["memory_peak_cgroup"], 200.0)
            self.assertEqual(metrics["memory_anon_peak"], 300.0)
            self.assertEqual(metrics["memory_file_peak"], 400.0)
            self.assertEqual(metrics["memory_kernel_peak"], 50.0)
            self.assertEqual(metrics["memory_sock_peak"], 6.0)
            self.assertEqual(metrics["pids_current_peak"], 2.0)
            self.assertEqual(metrics["threads_current_peak"], 8.0)

    def test_shard_merge_recomputes_counters_and_rates(self) -> None:
        """Ensure parallel source shards produce one coherent load result.

        Returns:
            None.

        Raises:
            AssertionError: If shard counters or rates are merged incorrectly.
        """

        shard = {
            "schema_version": 1,
            "mode": "tcp-connections",
            "elapsed_seconds": 2.0,
            "establishment_seconds": 1.0,
            "requested_connections": 5,
            "successful_connections": 4,
            "failed_connections": 1,
            "completed_operations": 0,
            "sent_bytes": 0,
            "received_bytes": 0,
            "sent_packets": 0,
            "received_packets": 0,
            "errors": 1,
            "success_rate": 0.8,
            "error_rate": 0.2,
            "packet_loss_pct": 0.0,
            "throughput_mbps": 0.0,
            "packets_per_second": 0.0,
            "connection_rate_per_second": 4.0,
            "offered_packets_per_second": 0,
            "p50_latency_us": None,
            "p95_latency_us": None,
            "p99_latency_us": None,
            "max_latency_us": None,
            "p50_connect_latency_us": None,
            "p95_connect_latency_us": None,
            "p99_connect_latency_us": 100,
            "max_connect_latency_us": 100,
        }
        with tempfile.TemporaryDirectory() as directory:
            paths = []
            for index in (1, 2):
                path = Path(directory) / f"shard-{index}.json"
                path.write_text(json.dumps(shard), encoding="utf-8")
                paths.append(path)
            result = merge(paths)
            self.assertEqual(result["requested_connections"], 10)
            self.assertEqual(result["successful_connections"], 8)
            self.assertAlmostEqual(result["success_rate"], 0.8)
            self.assertAlmostEqual(result["connection_rate_per_second"], 8.0)

    def test_shard_merge_reports_packet_loss_and_error_rate(self) -> None:
        """Ensure packet loss is derived from sent and received packets.

        Returns:
            None.

        Raises:
            AssertionError: If failed packets are omitted from the result.
        """

        shard = {
            "schema_version": 1,
            "mode": "udp-throughput",
            "elapsed_seconds": 2.0,
            "establishment_seconds": 0.001,
            "requested_connections": 1,
            "successful_connections": 1,
            "failed_connections": 0,
            "completed_operations": 8,
            "sent_bytes": 640,
            "received_bytes": 512,
            "sent_packets": 10,
            "received_packets": 8,
            "errors": 2,
            "success_rate": 0.8,
            "error_rate": 0.2,
            "packet_loss_pct": 20.0,
            "throughput_mbps": 0.002048,
            "packets_per_second": 4.0,
            "connection_rate_per_second": 1000.0,
            "offered_packets_per_second": 10,
            "p50_latency_us": None,
            "p95_latency_us": None,
            "p99_latency_us": None,
            "max_latency_us": None,
            "p50_connect_latency_us": None,
            "p95_connect_latency_us": None,
            "p99_connect_latency_us": None,
            "max_connect_latency_us": None,
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "shard.json"
            path.write_text(json.dumps(shard), encoding="utf-8")

            result = merge([path])

        self.assertAlmostEqual(result["error_rate"], 0.2)
        self.assertAlmostEqual(result["packet_loss_pct"], 20.0)

    def test_report_table_exposes_error_rate_as_a_percentage(self) -> None:
        """Ensure connection failures are visible in the human-readable report.

        Returns:
            None.

        Raises:
            AssertionError: If error rate is omitted or scaled incorrectly.
        """

        table = _normalized_table(
            {
                "normalized": [
                    {
                        "scenario_id": "sample",
                        "title": "Sample",
                        "target": "nginx",
                        "metrics": {"error_rate": 0.001, "packet_loss_pct": 2.5},
                    }
                ]
            }
        )

        self.assertIn("Error rate", table)
        self.assertIn("0.100%", table)
        self.assertIn("2.500%", table)

    def test_overview_contains_all_l4_panels_and_targets(self) -> None:
        """Ensure one overview figure contains every requested L4 dimension.

        Returns:
            None.

        Raises:
            AssertionError: If the overview omits a target or metric panel.
        """

        kinds = (
            "tcp_throughput",
            "udp_throughput",
            "udp_pps",
            "tcp_long_lived",
            "tcp_connect_rate",
            "udp_flows",
            "p99_latency",
        )
        normalized = []
        for index, kind in enumerate(kinds):
            for target, multiplier in (
                ("nginx", 1.0),
                ("caddy", 0.8),
                ("raddex-l4", 0.85),
                ("nat", 1.1),
            ):
                metrics = {
                    "throughput_pct": 100.0 * multiplier,
                    "pps_pct": 100.0 * multiplier,
                    "connection_rate_pct": 100.0 * multiplier,
                    "active_connections_pct": 100.0 * multiplier,
                    "p99_latency_pct": 100.0 * multiplier,
                    "cpu_pct": 100.0 * multiplier,
                    "memory_pct": 100.0 * multiplier,
                }
                normalized.append(
                    {
                        "scenario_id": f"scenario-{index}",
                        "title": f"{kind} / sample",
                        "kind": kind,
                        "target": target,
                        "metrics": metrics,
                    }
                )
        with tempfile.TemporaryDirectory() as directory:
            svg_path, png_path = create_overview_chart({"normalized": normalized}, Path(directory))
            self.assertTrue(svg_path.is_file())
            self.assertTrue(png_path.is_file())
            svg = svg_path.read_text(encoding="utf-8")
            for label in (
                "Nginx stream",
                "Caddy layer4",
                "Raddex L4",
                "Linux NAT / nftables",
            ):
                self.assertIn(label, svg)
            for label in ("TCP throughput", "UDP throughput", "packets per second", "p99 latency", "Peak memory"):
                self.assertIn(label, svg)


if __name__ == "__main__":
    unittest.main()
