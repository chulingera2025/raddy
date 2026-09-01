#!/usr/bin/env python3
"""Generate the unified Linux L4 forwarding benchmark report."""

from __future__ import annotations

import argparse
import csv
import html
import json
import math
import shutil
import sys
from pathlib import Path
from typing import Any

import matplotlib.pyplot as plt
from matplotlib.lines import Line2D
from matplotlib.patches import Patch


TARGETS = ("nginx", "caddy", "raddex-l4", "nat")
TARGET_LABELS = {
    "nginx": "Nginx stream",
    "caddy": "Caddy layer4",
    "raddex-l4": "Raddex L4",
    "nat": "Linux NAT / nftables",
}
TARGET_COLORS = {
    "nginx": "#5b6573",
    "caddy": "#2f80ed",
    "raddex-l4": "#e45756",
    "nat": "#7a5af8",
}
CHARTS = (
    {
        "kind": "tcp_throughput",
        "field": "throughput_pct",
        "title": "TCP throughput (Nginx = 100%; higher is better)",
        "ylabel": "Mi bit/s (% of Nginx)",
    },
    {
        "kind": "udp_throughput",
        "field": "throughput_pct",
        "title": "UDP throughput (Nginx = 100%; higher is better)",
        "ylabel": "Mi bit/s (% of Nginx)",
    },
    {
        "kind": "udp_pps",
        "field": "pps_pct",
        "title": "UDP packets per second (Nginx = 100%; higher is better)",
        "ylabel": "Packets/s (% of Nginx)",
    },
    {
        "kind": "tcp_long_lived",
        "field": "active_connections_pct",
        "title": "TCP concurrent connections (Nginx = 100%; higher is better)",
        "ylabel": "Established connections (% of Nginx)",
    },
    {
        "kind": "tcp_connect_rate",
        "field": "connection_rate_pct",
        "title": "TCP connection establishment rate (Nginx = 100%; higher is better)",
        "ylabel": "Connections/s (% of Nginx)",
    },
    {
        "kind": "tcp_long_lived",
        "field": "cpu_pct",
        "title": "Long-lived TCP CPU cost for user-space targets (Nginx = 100%; lower is better)",
        "ylabel": "CPU ms/connection (% of Nginx)",
    },
    {
        "kind": "udp_flows",
        "field": "active_connections_pct",
        "title": "UDP flow capacity (Nginx = 100%; higher is better)",
        "ylabel": "Established flows (% of Nginx)",
    },
    {
        "kind": "p99_latency",
        "field": "p99_latency_pct",
        "title": "p99 latency (Nginx = 100%; lower is better)",
        "ylabel": "p99 latency (% of Nginx)",
    },
    {
        "kind": None,
        "field": "memory_pct",
        "title": "Peak user-space memory (Nginx = 100%; lower is better)",
        "ylabel": "Peak memory (% of Nginx)",
    },
)
OVERVIEW_FILENAME = "overview"
PUBLIC_FILENAME = "l4-forwarding"


def load_summary(path: Path) -> dict[str, Any]:
    """Load and validate a normalized L4 summary.

    Args:
        path: Summary JSON emitted by ``collect.py``.

    Returns:
        Decoded summary object.

    Raises:
        ValueError: If normalized records are absent or structurally invalid.
        OSError: If the file cannot be read.
    """

    summary = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(summary, dict):
        raise ValueError(f"summary is not an object: {path}")
    normalized = summary.get("normalized")
    if not isinstance(normalized, list) or not normalized:
        raise ValueError(f"summary has no normalized L4 records: {path}")
    grouped: dict[str, set[str]] = {}
    for index, item in enumerate(normalized):
        if not isinstance(item, dict):
            raise ValueError(f"normalized record {index} is not an object: {path}")
        scenario_id = item.get("scenario_id")
        target = item.get("target")
        if not isinstance(scenario_id, str) or not scenario_id:
            raise ValueError(f"normalized record {index} has no scenario id: {path}")
        if target not in TARGETS:
            raise ValueError(f"unknown target {target!r} in {scenario_id}")
        if not isinstance(item.get("metrics"), dict):
            raise ValueError(f"missing metrics for {scenario_id} / {target}")
        if not isinstance(item.get("raw"), dict):
            raise ValueError(f"missing raw metrics for {scenario_id} / {target}")
        scenario_targets = grouped.setdefault(scenario_id, set())
        if target in scenario_targets:
            raise ValueError(f"duplicate target {target} for {scenario_id}")
        scenario_targets.add(target)
    for scenario_id, targets in grouped.items():
        missing = [target for target in TARGETS if target not in targets]
        if missing:
            raise ValueError(f"missing targets for {scenario_id}: {', '.join(missing)}")
    return summary


def _short_label(title: str) -> str:
    """Return a compact two-line label for one L4 scenario."""

    replacements = {
        "TCP throughput / 64 KiB / 1 connection": "TCP throughput\n64 KiB / 1",
        "TCP throughput / 64 KiB / 16 connections": "TCP throughput\n64 KiB / 16",
        "TCP throughput / 64 KiB / 64 connections": "TCP throughput\n64 KiB / 64",
        "UDP throughput / 64 B datagrams": "UDP throughput\n64 B",
        "UDP throughput / 512 B datagrams": "UDP throughput\n512 B",
        "UDP throughput / 1400 B datagrams": "UDP throughput\n1400 B",
        "UDP packets per second / 64 B datagrams": "UDP PPS\n64 B",
        "TCP p99 latency / 64 B / 1 connection": "TCP p99\n64 B / 1",
        "TCP p99 latency / 64 B / 16 connections": "TCP p99\n64 B / 16",
        "TCP p99 latency / 64 B / 64 connections": "TCP p99\n64 B / 64",
        "UDP p99 latency / 64 B datagrams": "UDP p99\n64 B",
        "TCP connection rate / 10K connections": "TCP connect\n10K",
        "TCP connection rate / 50K connections": "TCP connect\n50K",
        "TCP connection rate / 100K connections": "TCP connect\n100K",
        "TCP long-lived connections / 10K": "TCP long-lived\n10K",
        "TCP long-lived connections / 50K": "TCP long-lived\n50K",
        "TCP long-lived connections / 100K": "TCP long-lived\n100K",
        "UDP flows / 10K clients": "UDP flows\n10K",
        "UDP flows / 50K clients": "UDP flows\n50K",
        "UDP flows / 100K clients": "UDP flows\n100K",
    }
    return replacements.get(title, title.replace(" / ", "\n/ "))


def _chart_data(
    summary: dict[str, Any], kind: str | None, field: str
) -> tuple[list[str], dict[str, list[float | None]]]:
    """Extract one field for the requested scenario kind.

    Args:
        summary: Normalized benchmark summary.
        kind: Scenario kind, or ``None`` for every scenario.
        field: Normalized metric field.

    Returns:
        Scenario labels and one value list per target.

    Raises:
        ValueError: If no scenario or target value is available.
    """

    selected = [item for item in summary["normalized"] if kind is None or item["kind"] == kind]
    if not selected:
        raise ValueError(f"summary has no scenarios for {kind or 'all'} / {field}")
    scenarios: list[str] = []
    by_scenario: dict[str, dict[str, dict[str, Any]]] = {}
    for item in selected:
        scenario_id = item["scenario_id"]
        if scenario_id not in by_scenario:
            scenarios.append(scenario_id)
        by_scenario.setdefault(scenario_id, {})[item["target"]] = item
    values: dict[str, list[float | None]] = {target: [] for target in TARGETS}
    labels: list[str] = []
    for scenario_id in scenarios:
        targets = by_scenario[scenario_id]
        labels.append(_short_label(str(targets["nginx"]["title"])))
        for target in TARGETS:
            value = targets.get(target, {}).get("metrics", {}).get(field)
            if value is None:
                values[target].append(None)
                continue
            if not math.isfinite(float(value)):
                raise ValueError(f"missing {field} for {scenario_id} / {target}")
            values[target].append(float(value))
    return labels, values


def _axis_limits(values: list[float]) -> tuple[float, float]:
    """Choose a readable domain for one normalized metric panel.

    Args:
        values: All target values in the panel.

    Returns:
        Lower and upper y-axis bounds.

    Raises:
        ValueError: If values are empty, non-finite, or non-positive.
    """

    finite = [value for value in values if math.isfinite(value)]
    if not finite or max(finite) <= 0:
        raise ValueError("relative chart requires finite positive values")
    minimum = min(finite)
    maximum = max(finite)
    spread = maximum - minimum
    if spread / maximum < 0.15:
        padding = max(1.5, spread * 0.4, maximum * 0.015)
        lower = max(0.0, minimum - padding)
        upper = maximum + padding
    else:
        lower = 0.0
        upper = maximum * 1.1
    if upper <= lower:
        upper = lower + 1.0
    return lower, upper


def create_overview_chart(summary: dict[str, Any], output_dir: Path) -> tuple[Path, Path]:
    """Create the unified nine-panel L4 comparison chart.

    Args:
        summary: Normalized benchmark summary.
        output_dir: Directory receiving the overview SVG and PNG.

    Returns:
        Paths to the generated SVG and PNG files.

    Raises:
        ValueError: If a requested scenario metric is missing.
        OSError: If an output cannot be written.
    """

    figure, axes = plt.subplots(3, 3, figsize=(20, 17))
    for panel_index, chart in enumerate(CHARTS):
        axis = axes.flat[panel_index]
        labels, values = _chart_data(summary, chart["kind"], chart["field"])
        positions = list(range(len(labels)))
        all_values = [
            value for target in TARGETS for value in values[target] if value is not None
        ]
        lower, upper = _axis_limits(all_values)
        center = (len(TARGETS) - 1) / 2
        offsets = {
            target: (index - center) * 0.12 for index, target in enumerate(TARGETS)
        }
        for target in TARGETS:
            points = [
                (position + offsets[target], value)
                for position, value in zip(positions, values[target])
                if value is not None
            ]
            if points:
                axis.scatter(
                    [point[0] for point in points],
                    [point[1] for point in points],
                    s=34,
                    color=TARGET_COLORS[target],
                    edgecolors="white",
                    linewidths=0.6,
                    zorder=3,
                )
            for x_value, value in points:
                axis.annotate(
                    f"{value:.1f}",
                    (x_value, value),
                    xytext=(0, 5),
                    textcoords="offset points",
                    ha="center",
                    va="bottom",
                    fontsize=7,
                    color="#222222",
                )
        axis.axhline(100, color="#333333", linewidth=0.8, linestyle="--", zorder=1)
        axis.set_title(chart["title"], fontsize=11)
        axis.set_ylabel(chart["ylabel"], fontsize=9)
        axis.set_xticks(positions, labels, rotation=35, ha="right", fontsize=8)
        axis.set_xlim(-0.6, len(labels) - 0.4)
        axis.set_ylim(lower, upper)
        axis.grid(axis="y", alpha=0.25)
        axis.set_axisbelow(True)
        if lower > 0:
            axis.text(
                0.01,
                0.03,
                "Zoomed y-axis",
                transform=axis.transAxes,
                fontsize=7,
                color="#555555",
                va="bottom",
            )

    legend_handles = [
        Patch(facecolor=TARGET_COLORS[target], label=TARGET_LABELS[target]) for target in TARGETS
    ]
    legend_handles.append(
        Line2D([0], [0], color="#333333", linewidth=0.8, linestyle="--", label="Nginx baseline")
    )
    figure.suptitle(
        "L4 forwarding benchmark overview (Nginx = 100%; normalized per scenario)",
        fontsize=17,
    )
    figure.legend(
        handles=legend_handles,
        ncol=5,
        loc="lower center",
        bbox_to_anchor=(0.5, 0.027),
        frameon=False,
        fontsize=10,
    )
    figure.text(
        0.5,
        0.008,
        "Linux NAT CPU uses host softirq time; conntrack/slab counters are reported separately.",
        ha="center",
        va="bottom",
        fontsize=9,
        color="#555555",
    )
    figure.tight_layout(rect=(0, 0.06, 1, 0.95))
    output_dir.mkdir(parents=True, exist_ok=True)
    svg_path = output_dir / f"{OVERVIEW_FILENAME}.svg"
    png_path = output_dir / f"{OVERVIEW_FILENAME}.png"
    figure.savefig(svg_path, format="svg")
    figure.savefig(png_path, format="png", dpi=160)
    plt.close(figure)
    return svg_path, png_path


def _normalized_table(summary: dict[str, Any]) -> str:
    """Render normalized L4 metrics as a Markdown table."""

    rows = [
        "| Scenario | Target | Throughput | PPS | Connect/s | Connections | p99 | CPU | Memory | Error rate | Packet loss |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for item in summary["normalized"]:
        metrics = item["metrics"]
        def value(field: str, digits: int = 1, multiplier: float = 1.0) -> str:
            raw = metrics.get(field)
            if raw is None:
                return "—"
            number = float(raw) * multiplier
            if not math.isfinite(number):
                raise ValueError(f"non-finite {field} for {item['scenario_id']} / {item['target']}")
            return f"{number:.{digits}f}%"

        rows.append(
            f"| {item['title']} | {TARGET_LABELS[item['target']]} | {value('throughput_pct')} | "
            f"{value('pps_pct')} | {value('connection_rate_pct')} | {value('active_connections_pct')} | "
            f"{value('p99_latency_pct')} | {value('cpu_pct')} | {value('memory_pct')} | "
            f"{value('error_rate', 3, 100.0)} | {value('packet_loss_pct', 3)} |"
        )
    return "\n".join(rows)


def _kernel_state_table(summary: dict[str, Any]) -> str:
    """Render raw kernel state captured for Linux NAT scenarios."""

    rows = [
        "| Scenario | Conntrack entries | nf_conntrack objects | nf_conntrack bytes (approx.) | Host softirq ms |",
        "| --- | ---: | ---: | ---: | ---: |",
    ]
    for item in summary["normalized"]:
        if item["target"] != "nat":
            continue
        raw = item["raw"]

        def value(field: str, digits: int) -> str:
            number = raw.get(field)
            if number is None:
                return "—"
            number = float(number)
            if not math.isfinite(number):
                raise ValueError(f"non-finite {field} for {item['scenario_id']} / nat")
            return f"{number:.{digits}f}"

        rows.append(
            f"| {item['title']} | {value('peak_conntrack_count', 0)} | "
            f"{value('peak_nf_conntrack_objects', 0)} | "
            f"{value('peak_nf_conntrack_bytes', 0)} | "
            f"{value('host_softirq_ms', 1)} |"
        )
    return "\n".join(rows) if len(rows) > 2 else "No Linux NAT kernel-state records were captured."


def _cgroup_state_table(summary: dict[str, Any]) -> str:
    """Render optional cgroup v2 accounting captured for each target.

    Args:
        summary: Normalized L4 benchmark summary.

    Returns:
        A Markdown table of cgroup memory and task-accounting fields.

    Raises:
        ValueError: If a captured cgroup value is non-finite.
    """

    rows = [
        "| Scenario | Target | memory.current peak | memory.peak | anon | file | kernel | sock | pids | threads |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]

    def value(raw: Any, unit: str = "bytes") -> str:
        if raw is None or raw == 0:
            return "—"
        number = float(raw)
        if not math.isfinite(number):
            raise ValueError("non-finite cgroup accounting value")
        if unit == "bytes":
            return f"{number / (1024 ** 2):.1f} MiB"
        return f"{number:.0f}"

    for item in summary["normalized"]:
        raw = item["raw"]
        rows.append(
            f"| {item['title']} | {TARGET_LABELS[item['target']]} | "
            f"{value(raw.get('cgroup_memory_current_peak'))} | "
            f"{value(raw.get('cgroup_memory_peak_bytes'))} | "
            f"{value(raw.get('cgroup_memory_anon_peak_bytes'))} | "
            f"{value(raw.get('cgroup_memory_file_peak_bytes'))} | "
            f"{value(raw.get('cgroup_memory_kernel_peak_bytes'))} | "
            f"{value(raw.get('cgroup_memory_sock_peak_bytes'))} | "
            f"{value(raw.get('cgroup_pids_peak'), 'count')} | "
            f"{value(raw.get('cgroup_threads_peak'), 'count')} |"
        )
    return "\n".join(rows) if len(rows) > 2 else "No cgroup accounting records were captured."


def write_markdown(summary: dict[str, Any], output: Path) -> None:
    """Write the Markdown L4 report.

    Args:
        summary: Normalized L4 benchmark summary.
        output: Markdown output path.

    Returns:
        None.

    Raises:
        OSError: If the report cannot be written.
    """

    manifest = summary["manifest"]
    content = "\n".join(
        [
            "# L4 forwarding benchmark report",
            "",
            "This report compares Nginx stream, Caddy layer4, Raddex L4 on Pingora, Raddex L4 on native Tokio, and Linux NAT / nftables.",
            "Nginx is the per-scenario baseline: `100%` means `1.00x`.",
            "",
            f"- Profile: `{manifest['profile']}`",
            f"- Raddex commit: `{manifest['raddex_commit']}`",
            f"- Run ID: `{manifest['run_id']}`",
            f"- Kernel: `{manifest.get('kernel_release', 'unknown')}`",
            "",
            "## Overview",
            "",
            f"![L4 forwarding benchmark overview](charts/{OVERVIEW_FILENAME}.svg)",
            "",
            "Every panel uses its own scale. Throughput, PPS, connection rate, and established capacity are higher-is-better; p99 latency, CPU, and memory are lower-is-better.",
            "",
            "## Normalized results",
            "",
            _normalized_table(summary),
            "",
            "## Linux NAT kernel state",
            "",
            _kernel_state_table(summary),
            "",
            "The conntrack byte value is an approximate active-slab footprint, not process RSS.",
            "",
            "## Cgroup accounting",
            "",
            _cgroup_state_table(summary),
            "",
            "Cgroup memory fields are raw accounting signals and are not substituted for the normalized peak-memory metric.",
            "",
            "## Interpretation",
            "",
            "- Fixed-size TCP/UDP data scenarios measure forwarding work at the configured payload size.",
            "- Connection and flow scenarios measure successful objects held during the duration; they are not request throughput.",
            "- Linux NAT performs forwarding in the kernel. Its process cgroup RSS is not conntrack memory, so conntrack and slab counters are kept as separate raw fields.",
            "- Results from different machines must not be merged by absolute throughput or latency.",
            "",
        ]
    )
    output.write_text(content, encoding="utf-8")


def write_html(summary: dict[str, Any], output: Path) -> None:
    """Write a browsable self-contained report referencing the overview SVG.

    Args:
        summary: Normalized L4 benchmark summary.
        output: HTML output path.

    Returns:
        None.

    Raises:
        OSError: If the report cannot be written.
    """

    manifest = summary["manifest"]
    table = html.escape(_normalized_table(summary))
    kernel_table = html.escape(_kernel_state_table(summary))
    cgroup_table = html.escape(_cgroup_state_table(summary))
    content = f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>L4 forwarding benchmark report</title>
  <style>
    body {{ font-family: system-ui, sans-serif; margin: 2rem auto; max-width: 1400px; line-height: 1.5; }}
    img {{ max-width: 100%; border: 1px solid #ddd; }}
    code {{ background: #f2f2f2; padding: .1rem .3rem; }}
    pre {{ overflow-x: auto; }}
  </style>
</head>
<body>
  <h1>L4 forwarding benchmark report</h1>
  <p>Nginx is the baseline: <code>100%</code> means <code>1.00x</code>.</p>
  <ul>
    <li>Profile: <code>{html.escape(str(manifest['profile']))}</code></li>
    <li>Raddex commit: <code>{html.escape(str(manifest['raddex_commit']))}</code></li>
    <li>Run ID: <code>{html.escape(str(manifest['run_id']))}</code></li>
    <li>Kernel: <code>{html.escape(str(manifest.get('kernel_release', 'unknown')))}</code></li>
  </ul>
  <img src="charts/{OVERVIEW_FILENAME}.svg" alt="L4 forwarding benchmark overview">
  <h2>Normalized results</h2>
  <pre>{table}</pre>
  <h2>Linux NAT kernel state</h2>
  <pre>{kernel_table}</pre>
  <p>The conntrack byte value is an approximate active-slab footprint, not process RSS.</p>
  <h2>Cgroup accounting</h2>
  <pre>{cgroup_table}</pre>
  <p>Cgroup memory fields are raw accounting signals and are not substituted for the normalized peak-memory metric.</p>
</body>
</html>
"""
    output.write_text(content, encoding="utf-8")


def write_csv(summary: dict[str, Any], output: Path) -> None:
    """Write raw and normalized L4 measurements as CSV.

    Args:
        summary: Normalized L4 benchmark summary.
        output: CSV output path.

    Returns:
        None.

    Raises:
        OSError: If the CSV cannot be written.
    """

    fields = [
        "scenario_id",
        "title",
        "kind",
        "target",
        "transport",
        "connections",
        "payload_bytes",
        "throughput_mbps",
        "packets_per_second",
        "offered_packets_per_second",
        "connection_rate_per_second",
        "successful_connections",
        "requested_connections",
        "p99_latency_ms",
        "p99_connect_latency_ms",
        "cpu_avg_percent",
        "cpu_ms_total",
        "cpu_ms_per_operation",
        "cpu_ms_per_connection",
        "peak_memory_bytes",
        "peak_conntrack_count",
        "peak_nf_conntrack_objects",
        "peak_nf_conntrack_bytes",
        "cgroup_memory_current_peak",
        "cgroup_memory_peak_bytes",
        "cgroup_memory_anon_peak_bytes",
        "cgroup_memory_file_peak_bytes",
        "cgroup_memory_kernel_peak_bytes",
        "cgroup_memory_sock_peak_bytes",
        "cgroup_pids_peak",
        "cgroup_threads_peak",
        "host_cpu_ms",
        "host_softirq_ms",
        "error_rate",
        "packet_loss_pct",
        "throughput_pct",
        "pps_pct",
        "connection_rate_pct",
        "active_connections_pct",
        "p99_latency_pct",
        "cpu_pct",
        "memory_pct",
    ]
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=fields)
        writer.writeheader()
        for item in summary["normalized"]:
            row = {
                "scenario_id": item["scenario_id"],
                "title": item["title"],
                "kind": item["kind"],
                "target": item["target"],
                "transport": item["transport"],
                "connections": item["connections"],
                "payload_bytes": item["payload_bytes"],
                **item["raw"],
                **item["metrics"],
            }
            writer.writerow({field: row.get(field, "") for field in fields})


def generate_report(
    summary_path: Path,
    output_dir: Path,
    public_dir: Path,
    stable_bench_dir: Path | None = None,
) -> None:
    """Generate the L4 overview and synchronize stable copies.

    Args:
        summary_path: Collected summary JSON.
        output_dir: Run-specific output directory.
        public_dir: Documentation-site public asset directory.
        stable_bench_dir: Optional tracked L4 benchmark directory.

    Returns:
        None.

    Raises:
        ValueError: If the summary is incomplete.
        OSError: If an output cannot be written.
    """

    summary = load_summary(summary_path)
    chart_dir = output_dir / "charts"
    overview_svg, _ = create_overview_chart(summary, chart_dir)
    public_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(overview_svg, public_dir / f"{PUBLIC_FILENAME}.svg")
    if stable_bench_dir is not None:
        stable_bench_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(overview_svg, stable_bench_dir / f"{OVERVIEW_FILENAME}.svg")
    write_markdown(summary, output_dir / "report.md")
    write_html(summary, output_dir / "report.html")
    write_csv(summary, output_dir / "summary.csv")


def parse_args() -> argparse.Namespace:
    """Parse report command-line arguments.

    Returns:
        Parsed command-line namespace.

    Raises:
        SystemExit: If argparse rejects the command line.
    """

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--summary", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--public-dir", type=Path, required=True)
    parser.add_argument("--stable-bench-dir", type=Path)
    return parser.parse_args()


def main() -> int:
    """Run report generation and return a shell-friendly status code.

    Returns:
        Zero on success, otherwise one after printing a diagnostic.

    Raises:
        None: Expected input errors are converted into a non-zero status.
    """

    args = parse_args()
    try:
        generate_report(args.summary, args.output_dir, args.public_dir, args.stable_bench_dir)
        return 0
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"report error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
