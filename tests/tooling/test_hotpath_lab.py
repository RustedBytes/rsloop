"""Regression tests for experiment isolation and statistical decision rules."""

import hashlib
import importlib.util
import json
import random
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

pytestmark = pytest.mark.tooling

SPEC = importlib.util.spec_from_file_location(
    "hotpath_lab", Path(__file__).resolve().parents[2] / "scripts" / "hotpath_lab.py"
)
assert SPEC is not None and SPEC.loader is not None
lab = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(lab)


def test_orders_are_balanced_and_deterministic():
    orders = lab.balanced_orders(12, random.Random(1729))
    assert orders.count(["baseline", "candidate"]) == 6
    assert orders.count(["candidate", "baseline"]) == 6
    assert orders == lab.balanced_orders(12, random.Random(1729))
    with pytest.raises(ValueError, match="even"):
        lab.balanced_orders(7, random.Random(0))


def test_paired_estimate_uses_ratios_not_unpaired_medians():
    baseline = [1.0, 10.0, 100.0, 1000.0]
    candidate = [value * 0.9 for value in baseline]
    estimate = lab.paired_estimate(baseline, candidate, resamples=200)
    assert estimate["change_pct"] == pytest.approx(-10)
    assert estimate["ci_pct"] == pytest.approx([-10, -10])
    assert estimate["pairs"] == 4


@pytest.mark.parametrize(
    "baseline,candidate",
    [([1], [1]), ([1, 2], [1]), ([0, 1], [1, 1]), ([1, 2], [1, float("nan")])],
)
def test_invalid_samples_are_rejected(baseline, candidate):
    with pytest.raises(ValueError):
        lab.paired_estimate(baseline, candidate)


@pytest.mark.parametrize(
    "primary,guard,reliable,expected",
    [
        ((-4, -2), (-1, 2), True, "performance_gate_passed"),
        ((-4, 0.1), (-1, 2), True, "inconclusive"),
        ((-4, -2), (-1, 4), True, "inconclusive"),
        ((-4, -2), (4, 5), True, "reject_regression"),
        ((-4, -2), (-1, 2), False, "insufficient_measurement"),
    ],
)
def test_gate_requires_gain_and_bounded_regression(primary, guard, reliable, expected):
    estimates = {"tcp": {"ci_pct": primary}, "tasks": {"ci_pct": guard}}
    assert (
        lab.performance_decision(
            estimates,
            primary="tcp",
            minimum_gain=1.0,
            regression_budget=3.0,
            reliable=reliable,
        )
        == expected
    )


def test_ir_extraction_skips_embedded_bitcode_and_unrelated_functions(tmp_path):
    ir = tmp_path / "module.ll"
    ir.write_text(
        'module asm "' + "target" * 10000 + '"\n'
        "define void @unrelated() {\n  ret void\n}\n"
        "define void @target() {\n  ret void\n}\n"
        "define void @target_helper() {\n  ret void\n}\n",
        encoding="utf-8",
    )
    bodies = list(lab.extract_functions(ir, "target"))
    assert len(bodies) == 2
    assert all("module asm" not in body and "unrelated" not in body for body in bodies)


def test_hashing_does_not_require_python311_file_digest(tmp_path, monkeypatch):
    monkeypatch.delattr(hashlib, "file_digest", raising=False)
    payload = b"x" * (2 * 1024 * 1024 + 17)
    path = tmp_path / "binary"
    path.write_bytes(payload)
    assert lab.sha256(path) == hashlib.sha256(payload).hexdigest()


def test_packet_resolves_profile_metadata_without_debug_graph(tmp_path):
    ir = tmp_path / "module.ll"
    body = (
        "define void @target() #3 !dbg !99 !prof !7 !PGOFuncName !8 {\n  ret void\n}\n"
    )
    ir.write_text(
        body + "attributes #3 = { nounwind }\n"
        "attributes #4 = { cold }\n"
        '!7 = !{!"function_entry_count", i64 123}\n'
        '!8 = !{!"target"}\n'
        '!99 = !{!"unrelated debug graph"}\n',
        encoding="utf-8",
    )
    selected = "".join(lab.analysis_metadata(ir, [body]))
    assert "function_entry_count" in selected
    assert '"target"' in selected
    assert "attributes #3" in selected
    assert "attributes #4" not in selected
    assert "!99 =" not in selected


def make_artifact(tmp_path, mode="release"):
    artifact = tmp_path / mode
    package = artifact / "package" / "rsloop"
    package.mkdir(parents=True)
    extension = package / "_loop.fake"
    extension.write_bytes(b"binary")
    manifest = {
        "schema": lab.SCHEMA,
        "mode": mode,
        "python": sys.version,
        "python_executable": sys.executable,
        "extension": "package/rsloop/_loop.fake",
        "package_files": lab.inventory(artifact / "package"),
    }
    lab.write_json(artifact / "manifest.json", manifest)
    return artifact, extension, manifest


def test_modified_binary_is_rejected(tmp_path):
    artifact, extension, _ = make_artifact(tmp_path)
    lab.load_artifact(artifact)
    extension.write_bytes(b"different")
    with pytest.raises(ValueError, match="changed"):
        lab.load_artifact(artifact)


def test_different_python_abi_is_rejected(tmp_path):
    artifact, _, manifest = make_artifact(tmp_path)
    manifest["python"] = "different Python"
    lab.write_json(artifact / "manifest.json", manifest)
    with pytest.raises(ValueError, match="exact Python"):
        lab.load_artifact(artifact)


def test_instrumented_performance_comparison_is_rejected(tmp_path):
    baseline, _, _ = make_artifact(tmp_path)
    candidate, _, _ = make_artifact(tmp_path, "generate")
    with pytest.raises(ValueError, match="instrumented"):
        lab.compare(SimpleNamespace(baseline=baseline, candidate=candidate))


def test_wrong_import_path_is_rejected(tmp_path, monkeypatch):
    artifact, _, _ = make_artifact(tmp_path)
    monkeypatch.delitem(sys.modules, "rsloop", raising=False)
    monkeypatch.setattr(sys, "path", sys.path.copy())
    monkeypatch.setattr(
        lab.importlib,
        "import_module",
        lambda name: SimpleNamespace(__file__=str(tmp_path / "installed.pyd")),
    )
    with pytest.raises(ValueError, match="Wrong extension"):
        lab.import_artifact(artifact)


def test_holdout_differs_from_training():
    for name in lab.WORKLOADS:
        training = lab.workload_config(name, "training")
        holdout = lab.workload_config(name, "holdout")
        assert training["callbacks"] != holdout["callbacks"]
        assert training["task_batch_size"] != holdout["task_batch_size"]
        assert training["concurrency"] != holdout["concurrency"]
        assert training["bulk_bytes"] >= 64 * 1024 * 1024


def test_child_environment_is_not_polluted(monkeypatch):
    monkeypatch.setenv("LLVM_PROFILE_FILE", "old.profraw")
    monkeypatch.setenv("CARGO_ENCODED_RUSTFLAGS", "bad")
    monkeypatch.setenv("PYTHONPATH", "wrong-package")
    monkeypatch.setenv("RSLOOP_USE_FAST_STREAMS", "0")
    env = lab.clean_environment()
    assert (
        not {
            "LLVM_PROFILE_FILE",
            "CARGO_ENCODED_RUSTFLAGS",
            "PYTHONPATH",
            "RSLOOP_USE_FAST_STREAMS",
        }
        & env.keys()
    )
    assert env["PYO3_PYTHON"] == sys.executable
    assert env["PYTHONHASHSEED"] == "0"


def test_profile_override_is_only_applied_to_runtime(tmp_path, monkeypatch):
    calls = []

    def run(command, **kwargs):
        calls.append(kwargs)
        return SimpleNamespace(
            returncode=0, stdout=json.dumps({"result": {"seconds": 1}}), stderr=""
        )

    monkeypatch.setattr(lab.subprocess, "run", run)
    pattern = tmp_path / "%p-%m.profraw"
    lab.run_sample(tmp_path, {"workload": "tasks"}, 0, 30, pattern)
    assert calls[0]["env"]["LLVM_PROFILE_FILE"] == str(pattern)
    assert calls[0]["env"]["RSLOOP_USE_FAST_STREAMS"] == "1"


def test_test_fixtures_do_not_modify_artifact_source(tmp_path, monkeypatch):
    artifact, _, manifest = make_artifact(tmp_path)
    source = artifact / "source"
    source.mkdir()
    (source / "Cargo.toml").write_text("source placeholder", encoding="utf-8")
    manifest.update(source=str(source), source_files=lab.inventory(source))
    lab.write_json(artifact / "manifest.json", manifest)
    root = tmp_path / "repo"
    fixtures = root / "tests" / "fixtures" / "tls"
    fixtures.mkdir(parents=True)
    (fixtures / "test.pem").write_text("test data", encoding="utf-8")
    monkeypatch.setattr(lab, "ROOT", root)
    calls = []

    def run(command, **kwargs):
        calls.append((command, kwargs["env"].copy(), kwargs["cwd"]))
        return SimpleNamespace(returncode=0)

    monkeypatch.setattr(lab.subprocess, "run", run)
    log = tmp_path / "tests.log"
    lab.test_artifact(SimpleNamespace(artifact=artifact, python_child=False, log=log))
    assert not (source / "tests").exists()
    assert (
        tmp_path / "tests.source" / "tests" / "fixtures" / "tls" / "test.pem"
    ).is_file()
    assert str(tmp_path / "tests.source") in calls[0][1]["CARGO_ENCODED_RUSTFLAGS"]
    assert calls[1][1]["PYTHONPATH"] == str(artifact / "package")
    lab.verify_inventory(source, manifest["source_files"])


def test_compare_refuses_mismatched_codegen_flags(tmp_path):
    paths = []
    for index, flag in enumerate(("-Ctarget-cpu=generic", "-Ctarget-cpu=native")):
        artifact, _, manifest = make_artifact(tmp_path / str(index))
        manifest.update(rustc="same", cargo="same", platform="same", flags=[flag])
        lab.write_json(artifact / "manifest.json", manifest)
        paths.append(artifact)
    with pytest.raises(ValueError, match="non-PGO compiler flags"):
        lab.compare(SimpleNamespace(baseline=paths[0], candidate=paths[1]))


def test_compare_archives_harness_and_refuses_to_promote_short_experiment(
    tmp_path, monkeypatch
):
    manifest = {
        "mode": "release",
        "rustc": "same",
        "python": "same",
        "cargo": "same",
        "platform": "same",
        "flags": [],
    }
    monkeypatch.setattr(lab, "load_artifact", lambda path: manifest)
    monkeypatch.setattr(lab, "run_sample", lambda *args: {"result": {"seconds": 1.0}})
    args = SimpleNamespace(
        baseline=tmp_path / "baseline",
        candidate=tmp_path / "candidate",
        workloads="tcp_streams",
        primary="tcp_streams",
        out=tmp_path / "experiment",
        seed=0,
        blocks=2,
        suite="holdout",
        warmups=0,
        min_seconds=0.25,
        minimum_gain=1.0,
        regression_budget=3.0,
        timeout=10.0,
    )
    lab.compare(args)
    plan = lab.read_json(args.out / "plan.json")
    assert lab.sha256(args.out / "harness" / "hotpath_lab.py") == plan["runner_sha256"]
    for filename, digest in plan["benchmark_sha256"].items():
        assert lab.sha256(args.out / "harness" / filename) == digest
    assert (
        lab.read_json(args.out / "report.json")["decision"]
        == "insufficient_measurement"
    )
    assert (
        len((args.out / "samples.jsonl").read_text(encoding="utf-8").splitlines()) == 4
    )
    with pytest.raises(FileExistsError):
        lab.compare(args)
