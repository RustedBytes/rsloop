"""Statistics checks independent of optional native loop packages."""

import json
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "benches"))
from paired_comparison import distribution, summarize


class PairedStatisticsTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "linux", "Linux VmHWM accounting")
    def test_worker_peak_excludes_parent_memory_before_exec(self):
        # Force fork rather than posix_spawn: a large orchestrator must not
        # become the minimum reported RSS of every fresh benchmark worker.
        parent_memory = bytearray(64 * 1024 * 1024)
        command = [
            sys.executable,
            "-c",
            "import json, resource, sys; "
            f"sys.path.insert(0, {str(Path(__file__).resolve().parents[2] / 'benches')!r}); "
            "from paired_comparison import get_worker_peak_rss_bytes; "
            "print(json.dumps([get_worker_peak_rss_bytes(), "
            "resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024]))",
        ]
        child = subprocess.run(
            command, capture_output=True, text=True, check=True, preexec_fn=lambda: None
        )
        worker_peak, inherited_peak = json.loads(child.stdout)
        self.assertGreater(worker_peak, 0)
        self.assertGreater(inherited_peak - worker_peak, len(parent_memory) // 2)

    def test_nearest_rank_percentiles(self):
        result = distribution(list(range(1, 101)))
        self.assertEqual(result["median"], 50.5)
        self.assertEqual(result["p95"], 95)
        self.assertEqual(result["p99"], 99)

    def test_zero_mean_paired_changes(self):
        self.assertEqual(distribution([-10, 10])["cv_percent"], 0)
        self.assertEqual(distribution([-10, 10])["median"], 0)

    def test_throughput_is_median_of_trials(self):
        rows = [
            {
                "label": "pre",
                "workload": "callbacks",
                "result": {
                    "seconds": seconds,
                    "ops_per_sec": 10 / seconds,
                    "peak_rss_bytes": 100,
                },
            }
            for seconds in (1, 2, 4)
        ]
        summary = summarize(rows)
        self.assertEqual(len(summary), 1)
        self.assertEqual(summary[0]["seconds"]["median"], 2)
        self.assertEqual(summary[0]["ops_per_sec"]["median"], 5)

    def test_rtt_percentiles_are_not_pooled(self):
        rows = [
            {
                "label": "current",
                "workload": "tcp_latency",
                "result": {
                    "rtt_us": {"median": value, "p95": value * 2, "p99": value * 3},
                    "peak_rss_bytes": 100,
                },
            }
            for value in (10, 20, 30)
        ]
        result = summarize(rows)[0]
        self.assertEqual(result["rtt_us"]["p95"]["median"], 40)
        self.assertEqual(result["rtt_us"]["p99"]["median"], 60)
