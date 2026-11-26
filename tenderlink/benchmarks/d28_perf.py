#!/usr/bin/env python3
"""Sequential D28 trials in fresh, rootless Linux network namespaces.

No shell, installs, external traffic, sudo, or host-network fallback. See README.md.
"""

import argparse
import contextlib
import datetime
import hashlib
import io
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
BUILD_COMMAND = ["cargo", "build", "--manifest-path", "tenderlink/Cargo.toml",
                 "--release", "--features", "bench", "--bin", "d28_bench"]
MAX_SEED = 2**32 - 1
NS_PATTERN = re.compile(r"net:\[(\d+)\]\Z")


class TrialError(Exception):
    def __init__(self, kind, message):
        super().__init__(message)
        self.kind = kind


class TrialInterrupted(Exception):
    pass


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def write_json(path, value):
    # Every artifact is new. In particular, never update the Rust report in place.
    with Path(path).open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True, allow_nan=False)
        stream.write("\n")


def read_json(path):
    with Path(path).open(encoding="utf-8") as stream:
        return json.load(stream)


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def bounded_int(low, high):
    def parse(text):
        try:
            value = int(text)
        except (ValueError, TypeError):
            raise argparse.ArgumentTypeError("must be an integer") from None
        if not low <= value <= high:
            raise argparse.ArgumentTypeError(f"must be in [{low}, {high}]")
        return value
    return parse


def bounded_float(low, high):
    def parse(text):
        try:
            value = float(text)
        except (ValueError, TypeError):
            raise argparse.ArgumentTypeError("must be a number") from None
        if not math.isfinite(value) or not low <= value <= high:
            raise argparse.ArgumentTypeError(f"must be finite and in [{low}, {high}]")
        return value
    return parse


def case_name(text):
    if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_-]{0,47}", text):
        raise argparse.ArgumentTypeError("use 1..48 letters, digits, underscores or hyphens")
    return text


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="mode", required=True)
    tests = commands.add_parser("selftest", help="unit tests; no Rust binary needed")
    tests.add_argument("--netns", action="store_true", help="also probe real rootless netem with synthetic loopback UDP")
    summary = commands.add_parser("summary", help="read saved manifests and unchanged Rust reports")
    summary.add_argument("directory", type=Path)
    for mode in ("run", "matrix"):
        command = commands.add_parser(mode, help="one configurable case" if mode == "run"
                                      else "clean, delay, loss/jitter, aggregate rate, offline minority")
        command.add_argument("--out", required=True, type=Path, help="NEW output directory; never reused")
        command.add_argument("--binary", type=Path, default=ROOT / "tenderlink/target/release/d28_bench")
        command.add_argument("--nodes", type=bounded_int(1, 128), default=4)
        command.add_argument("--offline", type=bounded_int(0, 127), default=0,
                             help="run only: validators absent for the entire trial")
        command.add_argument("--heights", type=bounded_int(1, 10000), default=30,
                             help="measured heights; heights + warmup must be <= 10000")
        command.add_argument("--warmup", type=bounded_int(0, 10000), default=3)
        command.add_argument("--headers", type=bounded_int(0, 2048), default=32,
                             help="synthetic Zcash headers per body; Rust measures serialized bytes")
        command.add_argument("--max-body", type=bounded_int(1, 16777216),
                             help="explicit Rust body cap override; omitted by default")
        command.add_argument("--timeout-secs", type=bounded_int(1, 86400), default=180,
                             help="Rust run deadline; production protocol timers stay unchanged")
        command.add_argument("--hard-timeout-secs", type=bounded_float(1, 90120),
                             help="whole trial deadline (default: Rust timeout + chatter tail + 20s)")
        command.add_argument("--chatter-secs", type=bounded_int(0, 3600), default=1,
                             help="separate final-height traffic hold; default 1s (Rust default is 0s)")
        command.add_argument("--workers", type=bounded_int(1, 128), default=4)
        command.add_argument("--host-delay-ms", type=bounded_int(0, 60000), default=0)
        command.add_argument("--store-delay-ms", type=bounded_int(0, 60000), default=0)
        command.add_argument("--seed", type=bounded_int(1, MAX_SEED), default=1,
                             help="netem seed; incremented for each repetition")
        command.add_argument("--repetitions", type=bounded_int(1, 100), default=1)
        command.add_argument("--queue-packets", type=bounded_int(1, 1000000), default=10000,
                             help="shared netem queue limit, in packets")
        command.add_argument("--mtu", type=bounded_int(1280, 65535), default=1500,
                             help="private loopback MTU; default 1500 instead of Linux lo's large default")
        command.add_argument("--host-note", default="Shared host; other activity was not excluded.",
                             help="record scheduling/load conditions, not an isolation claim")
        if mode == "run":
            command.add_argument("--case", type=case_name, default="custom")
            command.add_argument("--delay-ms", type=bounded_float(0, 60000), default=0)
            command.add_argument("--jitter-ms", type=bounded_float(0, 60000), default=0)
            command.add_argument("--loss-pct", type=bounded_float(0, 100), default=0)
            command.add_argument("--rate-mbit", type=bounded_float(0.001, 1000000),
                                 help="AGGREGATE lo budget across all validators; not per-node uplink")
    return result


def validate_args(args):
    if args.mode not in ("run", "matrix"):
        return
    if args.offline >= args.nodes or 3 * (args.nodes - args.offline) <= 2 * args.nodes:
        raise ValueError("online validators must retain >2/3 quorum (3*(nodes-offline) > 2*nodes)")
    if args.heights + args.warmup > 10000:
        raise ValueError("--heights plus --warmup must be <= 10000")
    if args.seed + args.repetitions - 1 > MAX_SEED:
        raise ValueError("--seed plus repetitions exceeds the netem uint32 seed range")
    if args.mode == "matrix" and (args.nodes < 4 or args.offline):
        raise ValueError("matrix needs --nodes >= 4 and --offline 0; its offline case is automatic")
    if args.mode == "run" and args.jitter_ms > args.delay_ms:
        raise ValueError("--jitter-ms must not exceed --delay-ms")
    if args.hard_timeout_secs is None:
        args.hard_timeout_secs = args.timeout_secs + args.chatter_secs + 20
    if len(args.host_note) > 4096:
        raise ValueError("--host-note must be at most 4096 characters")


def namespace_identity():
    path = "/proc/self/ns/net"
    return {"link": os.readlink(path), "inode": os.stat(path).st_ino}


def require_private_namespace(parent, current=None):
    current = namespace_identity() if current is None else current
    for value in (parent, current):
        match = NS_PATTERN.fullmatch(value.get("link", ""))
        if match is None or int(match[1]) != value.get("inode"):
            raise TrialError("isolation_refused", "invalid network namespace identity")
    if parent["link"] == current["link"] or parent["inode"] == current["inode"]:
        raise TrialError("isolation_refused", "still in parent network namespace; refusing ip/tc")
    return current


def number(value):
    return format(value, ".12g")


def netem_command(tc, case, seed, queue_packets):
    command = [tc, "qdisc", "add", "dev", "lo", "root", "netem", "limit", str(queue_packets)]
    if case["delay_ms"]:
        command += ["delay", number(case["delay_ms"]) + "ms"]
        if case["jitter_ms"]:
            # Uniform jitter, tc's default; no implicit normal distribution table.
            command += [number(case["jitter_ms"]) + "ms"]
    if case["loss_pct"]:
        command += ["loss", "random", number(case["loss_pct"]) + "%"]
    if case["rate_mbit"] is not None:
        command += ["rate", number(case["rate_mbit"]) + "mbit"]
    return command + ["seed", str(seed)]


def benchmark_command(config):
    command = [config["binary"]]
    for key, value in config["bench"].items():
        if value is not None:
            command += ["--" + key.replace("_", "-"), str(value)]
    return command + ["--json", str(Path(config["directory"]) / "report.json")]


def namespace_command(config_path, unshare):
    return [unshare, "--user", "--map-root-user", "--net", "--", sys.executable,
            str(Path(__file__).resolve()), "_trial", str(config_path)]


def parse_lo_counters(text):
    for line in text.splitlines():
        name, sep, fields = line.partition(":")
        if sep and name.strip() == "lo":
            values = fields.split()
            if len(values) != 16:
                break
            try:
                counters = {"tx_bytes": int(values[8]), "tx_packets": int(values[9]),
                            "tx_errors": int(values[10]), "tx_dropped": int(values[11])}
            except ValueError:
                break
            if min(counters.values()) >= 0:
                return counters
            break
    raise TrialError("counter_failure", "missing or malformed lo counters in /proc/net/dev")


def lo_counters():
    # /sys/class/net describes the host mount without a mount namespace. Never use it.
    return parse_lo_counters(Path("/proc/net/dev").read_text(encoding="ascii"))


def counter_delta(start, end):
    if start.keys() != end.keys() or any(end[key] < start[key] for key in start):
        raise TrialError("counter_failure", "counter set changed or counters moved backwards")
    return {key: end[key] - start[key] for key in start}


def captured_command(command, directory, label, timeout=10, env=None):
    """Worker commands inherit its private process group and network namespace."""
    started = time.monotonic()
    with (directory / (label + ".stdout.txt")).open("xb") as stdout, \
            (directory / (label + ".stderr.txt")).open("xb") as stderr:
        try:
            result = subprocess.run(command, stdout=stdout, stderr=stderr, timeout=timeout, env=env)
            record = {"argv": command, "returncode": result.returncode, "timed_out": False}
        except subprocess.TimeoutExpired:
            record = {"argv": command, "returncode": None, "timed_out": True}
        except OSError as exc:
            record = {"argv": command, "returncode": None, "timed_out": False, "error": str(exc)}
    record["elapsed_seconds"] = time.monotonic() - started
    write_json(directory / (label + ".command.json"), record)
    return record


def checked_command(command, directory, label):
    record = captured_command(command, directory, label)
    if record["returncode"] != 0:
        raise TrialError("setup_failed", f"{label} failed; see {label}.stderr.txt")
    return record


def qdisc_counters(snapshot, kind, keys=("bytes", "packets")):
    matches = [item for item in snapshot["qdisc"] if item.get("kind") == kind]
    if len(matches) != 1:
        raise TrialError("counter_failure", f"expected exactly one {kind} qdisc")
    counters = {key: matches[0].get(key) for key in keys}
    if any(type(value) is not int or value < 0 for value in counters.values()):
        raise TrialError("counter_failure", f"invalid {kind} qdisc counters")
    return counters


def pre_netem_delta(start, end):
    delta = counter_delta(qdisc_counters(start, "clsact"), qdisc_counters(end, "clsact"))
    estimated_l3 = delta["bytes"] - 14 * delta["packets"]
    if estimated_l3 < 0:
        raise TrialError("counter_failure", "clsact bytes do not fit the loopback 14-byte link-header estimate")
    return {"accounted_bytes": delta["bytes"], "packets": delta["packets"],
            "estimated_l3_bytes": estimated_l3,
            "estimate_note": "clsact bytes minus 14 synthetic loopback link bytes per packet; not retransmission bytes"}


def tc_snapshot(tc, directory, label):
    started = time.monotonic_ns()
    checked_command([tc, "-j", "-s", "qdisc", "show", "dev", "lo"], directory, label)
    checked_command([tc, "-j", "-s", "filter", "show", "dev", "lo", "egress"],
                    directory, label + "-filter")
    result = {"qdisc": read_json(directory / (label + ".stdout.txt")),
              "egress_filter": read_json(directory / (label + "-filter.stdout.txt")),
              "sample_started_monotonic_ns": started,
              "sample_elapsed_seconds": (time.monotonic_ns() - started) / 1e9}
    if not isinstance(result["qdisc"], list) or not isinstance(result["egress_filter"], list):
        raise TrialError("counter_failure", "tc JSON must contain arrays")
    qdisc_counters(result, "netem")
    qdisc_counters(result, "clsact")
    return result


def classify(returncode, timed_out, worker, report):
    if timed_out:
        return "hard_timeout"
    if worker is None:
        return "isolation_launch_failed"
    if worker.get("status") != "complete":
        return worker.get("failure_kind", "worker_failed")
    if report is None:
        return "report_missing_or_invalid"
    if not isinstance(report, dict) or report.get("schema_version") != 1:
        return "unsupported_report_schema"
    if report.get("status") != "complete":
        return "benchmark_partial" if report.get("status") == "partial" else "benchmark_failed"
    if returncode != 0 or worker.get("benchmark_returncode") != 0:
        return "benchmark_failed"
    return "complete"


def trial_worker(config_path):
    """Only the unshare child enters here; reject the parent's namespace first."""
    config = read_json(config_path)
    directory = Path(config["directory"])
    result = {"status": "failed", "started_at": utc_now(), "benchmark_returncode": None}
    installed = False
    before = None
    began = time.monotonic()

    def interrupted(signum, _frame):
        raise TrialInterrupted(f"received signal {signum}")

    old_term = signal.signal(signal.SIGTERM, interrupted)
    old_int = signal.signal(signal.SIGINT, interrupted)
    try:
        current = require_private_namespace(config["parent_namespace"])
        if os.environ.get("D28_BENCH_NETNS_PARENT") != config["parent_namespace"]["link"]:
            raise TrialError("isolation_refused", "parent namespace environment marker mismatch")
        write_json(directory / "isolation.json", {"parent": config["parent_namespace"], "child": current})
        if sha256(config["binary"]) != config["binary_sha256"]:
            raise TrialError("binary_changed", "benchmark binary changed since manifest creation")
        checked_command([config["tools"]["ip"], "link", "set", "dev", "lo", "mtu", str(config["mtu"])],
                        directory, "lo-mtu")
        checked_command([config["tools"]["ip"], "link", "set", "dev", "lo", "up"], directory, "lo-up")
        checked_command([config["tools"]["ip"], "-j", "link", "show", "dev", "lo"], directory, "lo-link")
        link = read_json(directory / "lo-link.stdout.txt")
        if not isinstance(link, list) or len(link) != 1 or link[0].get("mtu") != config["mtu"]:
            raise TrialError("setup_failed", "private lo MTU did not match requested value")
        result["loopback_mtu"] = link[0]["mtu"]
        # Guard immediately before the first tc operation, not only before ip.
        require_private_namespace(config["parent_namespace"])
        checked_command([config["tools"]["tc"], "qdisc", "add", "dev", "lo", "clsact"],
                        directory, "clsact-add")
        checked_command([config["tools"]["tc"], "filter", "add", "dev", "lo", "egress", "matchall",
                         "action", "gact", "pass"], directory, "egress-pass-add")
        checked_command(netem_command(config["tools"]["tc"], config["case"], config["seed"],
                                      config["queue_packets"]), directory, "netem-add")
        installed = True
        checked_command([config["tools"]["tc"], "-V"], directory, "tc-version")
        result["tc_before"] = tc_snapshot(config["tools"]["tc"], directory, "tc-before")
        before = lo_counters()
        write_json(directory / "lo-before.json", before)
        command = benchmark_command(config)
        # The outer watchdog owns this complete process group. No detached Rust process.
        with (directory / "benchmark.stdout.txt").open("xb") as stdout, \
                (directory / "benchmark.stderr.txt").open("xb") as stderr:
            child = subprocess.Popen(command, stdout=stdout, stderr=stderr)
            try:
                result["benchmark_returncode"] = child.wait()
            finally:
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
        result["status"] = "complete"
    except TrialError as exc:
        result.update(failure_kind=exc.kind, error=str(exc))
    except TrialInterrupted as exc:
        result.update(failure_kind="interrupted", error=str(exc))
    except (OSError, ValueError, KeyError, TypeError) as exc:
        result.update(failure_kind="worker_failed", error=f"{type(exc).__name__}: {exc}")
    finally:
        # SIGTERM permits a short evidence-collection tail before the outer SIGKILL.
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        if installed:
            try:
                require_private_namespace(config["parent_namespace"])
                result["tc_after"] = tc_snapshot(config["tools"]["tc"], directory, "tc-after")
                after = lo_counters()
                write_json(directory / "lo-after.json", after)
                result["network_whole_process"] = {
                    "start": before, "end": after,
                    "delta": counter_delta(before, after) if before is not None else None,
                    "scope": "Rust startup through process exit, including warmup, chatter tail and shutdown; no extra queue drain",
                }
                if "tc_before" in result:
                    result["network_whole_process"]["pre_netem"] = pre_netem_delta(result["tc_before"], result["tc_after"])
                    keys = ("bytes", "packets", "drops", "overlimits", "requeues")
                    result["network_whole_process"]["netem_delta"] = counter_delta(
                        qdisc_counters(result["tc_before"], "netem", keys),
                        qdisc_counters(result["tc_after"], "netem", keys))
            except (TrialError, OSError, ValueError, KeyError, TypeError) as exc:
                result.update(status="failed", failure_kind="evidence_failed", evidence_error=str(exc))
        result["elapsed_seconds"] = time.monotonic() - began
        result["finished_at"] = utc_now()
        write_json(directory / "worker.json", result)
        signal.signal(signal.SIGTERM, old_term)
        signal.signal(signal.SIGINT, old_int)
    return 0 if result["status"] == "complete" else 1


def terminate_group(child):
    """Only the session/group created by Popen(start_new_session=True)."""
    try:
        os.killpg(child.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        child.wait(timeout=3)
    except subprocess.TimeoutExpired:
        pass
    # Kill lingering descendants even if the group's leader already exited.
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    child.wait()


def optional_json(path):
    try:
        return read_json(path)
    except (OSError, ValueError):
        return None


def launch_trial(config):
    directory = Path(config["directory"])
    config_path = directory / "trial.json"
    config["benchmark_argv"] = benchmark_command(config)
    command = namespace_command(config_path, config["tools"]["unshare"])
    config["namespace_argv"] = command
    write_json(config_path, config)
    env = os.environ.copy()
    env["D28_BENCH_NETNS_PARENT"] = config["parent_namespace"]["link"]
    started = time.monotonic()
    result = {"started_at": utc_now(), "timed_out": False, "interrupted": False, "returncode": None}
    with (directory / "namespace.stdout.txt").open("xb") as stdout, \
            (directory / "namespace.stderr.txt").open("xb") as stderr:
        try:
            child = subprocess.Popen(command, stdout=stdout, stderr=stderr,
                                     start_new_session=True, env=env)
            try:
                result["returncode"] = child.wait(timeout=config["hard_timeout_secs"])
            except subprocess.TimeoutExpired:
                result["timed_out"] = True
                terminate_group(child)
                result["returncode"] = child.returncode
            except KeyboardInterrupt:
                result["interrupted"] = True
                terminate_group(child)
                result["returncode"] = child.returncode
        except OSError as exc:
            result["error"] = str(exc)
    worker = optional_json(directory / "worker.json")
    report = optional_json(directory / "report.json")
    result["status"] = ("interrupted" if result["interrupted"] else
                        classify(result["returncode"], result["timed_out"], worker, report))
    result["elapsed_seconds"] = time.monotonic() - started
    result["finished_at"] = utc_now()
    if (directory / "report.json").is_file():
        result["report_sha256"] = sha256(directory / "report.json")
    write_json(directory / "result.json", result)
    return result


def host_metadata(binary, tools, note):
    git = shutil.which("git")

    def git_capture(args):
        if git is None:
            return None
        try:
            result = subprocess.run([git, *args], cwd=ROOT, capture_output=True,
                                    text=True, timeout=10, check=False)
            return {"returncode": result.returncode, "stdout": result.stdout, "stderr": result.stderr}
        except (OSError, subprocess.TimeoutExpired) as exc:
            return {"error": str(exc)}

    cpu = None
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name") or line.startswith("Hardware"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    return {
        "created_at": utc_now(), "kernel": platform.uname()._asdict(),
        "cpu_model": cpu, "logical_cpu_count": os.cpu_count(),
        "cpu_affinity": sorted(os.sched_getaffinity(0)), "load_average": list(os.getloadavg()),
        "python": sys.version, "python_executable": sys.executable,
        "git_head": git_capture(["rev-parse", "HEAD"]),
        "git_tenderlink_status": git_capture(["status", "--porcelain=v1", "--untracked-files=normal", "--", "tenderlink"]),
        "binary": str(binary), "binary_sha256": sha256(binary), "binary_bytes": binary.stat().st_size,
        "expected_build_command": BUILD_COMMAND,
        "build_command_note": "Recorded build recipe, not proof that this invocation produced the binary.",
        "tools": tools, "host_note": note,
        "scheduling": "Trials sequential within this invocation; no exclusion of other host workloads.",
        "network": "Fresh rootless netns, private lo only; aggregate netem rate; no host-network fallback.",
        "protocol_timers": "Production defaults; wrapper does not override protocol timers.",
    }


def cases_for(args):
    def case(name, delay=0, jitter=0, loss=0, rate=None, offline=0):
        return dict(name=name, delay_ms=delay, jitter_ms=jitter, loss_pct=loss,
                    rate_mbit=rate, offline=offline)
    if args.mode == "run":
        return [case(args.case, args.delay_ms, args.jitter_ms, args.loss_pct, args.rate_mbit, args.offline)]
    return [case("clean"), case("delay25", delay=25),
            case("jitter_loss", delay=25, jitter=5, loss=1),
            case("rate10", rate=10), case("offline_minority", offline=(args.nodes - 1) // 3)]


def run_trials(args):
    if sys.platform != "linux":
        raise ValueError("Linux is required; refusing host-network fallback")
    tools = {name: shutil.which(name) for name in ("unshare", "ip", "tc")}
    missing = [name for name, path in tools.items() if path is None]
    if missing:
        raise ValueError("required existing tools not found: " + ", ".join(missing))
    binary = args.binary.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError(f"not an executable file: {binary}")
    parent = namespace_identity()
    output = args.out.resolve()
    # mkdir without exist_ok is the no-overwrite gate, including for empty dirs.
    output.mkdir(parents=True)
    metadata = host_metadata(binary, tools, args.host_note)
    cases = cases_for(args)
    bench = {key: getattr(args, key) for key in ("nodes", "offline", "heights", "warmup", "headers",
             "timeout_secs", "workers", "host_delay_ms", "store_delay_ms", "chatter_secs", "max_body")}
    manifest = {"wrapper_schema_version": 1, "host": metadata, "parent_namespace": parent,
                "bench": bench, "cases": cases, "repetitions": args.repetitions,
                "first_seed": args.seed, "hard_timeout_secs": args.hard_timeout_secs,
                "queue_packets": args.queue_packets, "mtu": args.mtu, "argv": sys.argv}
    write_json(output / "manifest.json", manifest)
    results = []
    interrupted = False
    for repetition in range(1, args.repetitions + 1):
        for case in cases:
            directory = output / f"{case['name']}-r{repetition:03d}"
            directory.mkdir()
            config = {"directory": str(directory), "binary": str(binary),
                      "binary_sha256": metadata["binary_sha256"], "parent_namespace": parent,
                      "bench": dict(bench, offline=case["offline"]), "case": case,
                      "repetition": repetition, "seed": args.seed + repetition - 1,
                      "queue_packets": args.queue_packets, "hard_timeout_secs": args.hard_timeout_secs,
                      "mtu": args.mtu, "tools": tools}
            result = launch_trial(config)
            results.append({"trial": directory.name, "status": result["status"]})
            print(f"{directory.name}: {result['status']}", flush=True)
            if result["interrupted"]:
                interrupted = True
                break
        if interrupted:
            break
    write_json(output / "run.json", {"finished_at": utc_now(), "trials": results})
    summarize(output)
    return 130 if interrupted else (0 if all(row["status"] == "complete" for row in results) else 1)


def nested(value, *keys):
    for key in keys:
        if not isinstance(value, dict):
            return None
        value = value.get(key)
    return value


def display_number(value):
    if isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value):
        return f"{value:.3f}"
    return "-"


def summary_row(directory):
    result = optional_json(directory / "result.json")
    report = optional_json(directory / "report.json")
    row = {"trial": directory.name, "status": nested(result, "status") or "no_result"}
    if not isinstance(report, dict) or report.get("schema_version") != 1:
        report = {}
    paths = {
        "completed_n": ("summary", "measured_heights"),
        "heights/s": ("summary", "committed_heights_per_second"),
        "body_B/s": ("summary", "committed_body_bytes_per_second"),
        "payload_B/s": ("summary", "committed_payload_bytes_per_second"),
    }
    for cohort in ("first", "quorum", "all"):
        paths[cohort + "_n"] = ("summary", "latency_samples", cohort)
        for percentile in ("p50", "p95", "p99"):
            paths[f"{cohort}_{percentile}_ms"] = ("summary", "latency_ms", cohort, percentile)
    for prefix, key in (("measured", "network"), ("tail", "chatter_network")):
        paths.update({
            prefix + "_lo_TX_B": (key, "delta", "tx_bytes"),
            prefix + "_lo_TX_pkt": (key, "delta", "tx_packets"),
            prefix + "_pre_L2_B": (key, "tc_delta", "pre_impairment_accounted_bytes"),
            prefix + "_pre_pkt": (key, "tc_delta", "pre_impairment_packets"),
            prefix + "_drops": (key, "tc_delta", "netem_drops"),
        })
    row.update({name: display_number(nested(report, *path)) for name, path in paths.items()})
    return row


def print_table(title, fields, rows):
    print("\n" + title)
    values = [fields, *[[row[field] for field in fields] for row in rows]]
    widths = [max(len(str(row[i])) for row in values) for i in range(len(fields))]
    for row in values:
        print("  ".join(str(value).ljust(width) for value, width in zip(row, widths)))


def summarize(directory):
    directory = directory.resolve(strict=True)
    if not (directory / "manifest.json").is_file():
        raise ValueError(f"not a D28 wrapper output directory: {directory}")
    rows = [summary_row(path.parent) for path in sorted(directory.glob("*/trial.json"))]
    latency_rows = []
    for row in rows:
        for cohort in ("first", "quorum", "all"):
            latency_rows.append(dict(trial=row["trial"], status=row["status"], cohort=cohort,
                                     samples=row[cohort + "_n"],
                                     **{p + "_ms": row[f"{cohort}_{p}_ms"] for p in ("p50", "p95", "p99")}))
    print_table("Decision latency (ms; failed/partial trials and per-cohort sample counts retained)",
                ["trial", "status", "cohort", "samples", "p50_ms", "p95_ms", "p99_ms"], latency_rows)
    print_table("Measured chain throughput and traffic (warmup excluded)",
                ["trial", "completed_n", "heights/s", "body_B/s", "payload_B/s", "measured_pre_L2_B",
                 "measured_pre_pkt", "measured_lo_TX_B", "measured_lo_TX_pkt", "measured_drops"], rows)
    print_table("Separate final-height hold traffic (not quiescent-chain idle traffic)",
                ["trial", "tail_pre_L2_B", "tail_pre_pkt", "tail_lo_TX_B", "tail_lo_TX_pkt", "tail_drops"], rows)
    print("First: one certificate-backed decision; quorum: >2/3 validators' local decisions; all: all online.")
    print("Chain rates count one chain, paced by the all-online ACK barrier; not network goodput.")
    print("Pre L2 includes 14 synthetic lo header bytes/packet; lo TX is post-netem L3 and excludes netem drops.")
    print("Neither is application-offered or retransmission-only bytes. '-' = unavailable; small-N p95/p99 are unstable.")


class SelfTests(unittest.TestCase):
    def parse(self, *args):
        with contextlib.redirect_stderr(io.StringIO()):
            return parser().parse_args(args)

    def test_argument_bounds(self):
        for option, value in (("--nodes", "0"), ("--offline", "-1"), ("--headers", "2049"),
                              ("--heights", "0"), ("--loss-pct", "101"), ("--delay-ms", "nan"),
                              ("--jitter-ms", "inf"), ("--rate-mbit", "0"),
                              ("--seed", str(MAX_SEED + 1)), ("--case", "../../escape"),
                              ("--hard-timeout-secs", "-1"), ("--workers", "129"),
                              ("--chatter-secs", "3601"), ("--max-body", "16777217"),
                              ("--mtu", "1279"), ("--mtu", "65536")):
            with self.subTest(option=option), self.assertRaises(SystemExit):
                self.parse("run", "--out", "new", option, value)
        for extra in (("--offline", "2"), ("--delay-ms", "1", "--jitter-ms", "2"),
                      ("--heights", "10000", "--warmup", "1"),
                      ("--seed", str(MAX_SEED), "--repetitions", "2")):
            with self.assertRaises(ValueError):
                validate_args(self.parse("run", "--out", "new", *extra))
        with self.assertRaises(ValueError):
            validate_args(self.parse("matrix", "--out", "new", "--nodes", "3"))

    def test_namespace_refusal(self):
        parent = {"link": "net:[123]", "inode": 123}
        for current in (parent, {"link": "net:[456]", "inode": 123},
                        {"link": "garbage", "inode": 456}):
            with self.assertRaises(TrialError):
                require_private_namespace(parent, current)
        child = {"link": "net:[456]", "inode": 456}
        self.assertEqual(require_private_namespace(parent, child), child)

    def test_refusal_precedes_all_commands(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            directory = Path(temporary)
            parent = {"link": "net:[123]", "inode": 123}
            config = directory / "trial.json"
            write_json(config, {"directory": temporary, "parent_namespace": parent})
            with mock.patch(__name__ + ".namespace_identity", return_value=parent), \
                    mock.patch(__name__ + ".captured_command") as command:
                self.assertEqual(trial_worker(config), 1)
                command.assert_not_called()
            self.assertEqual(read_json(directory / "worker.json")["failure_kind"], "isolation_refused")

    def test_commands(self):
        case = dict(delay_ms=25, jitter_ms=5, loss_pct=1, rate_mbit=10)
        command = netem_command("/sbin/tc", case, 42, 10000)
        self.assertEqual(command, ["/sbin/tc", "qdisc", "add", "dev", "lo", "root", "netem",
                                   "limit", "10000", "delay", "25ms", "5ms", "loss", "random", "1%",
                                   "rate", "10mbit", "seed", "42"])
        self.assertEqual(namespace_command(Path("/some space/trial.json"), "/bin/unshare")[:5],
                         ["/bin/unshare", "--user", "--map-root-user", "--net", "--"])
        config = {"binary": "/binary path/bench", "directory": "/out path", "bench": {"nodes": 4, "max_body": None}}
        self.assertEqual(benchmark_command(config), ["/binary path/bench", "--nodes", "4", "--json", "/out path/report.json"])
        clean = dict(delay_ms=0, jitter_ms=0, loss_pct=0, rate_mbit=None)
        self.assertEqual(netem_command("tc", clean, 1, 123)[-4:], ["limit", "123", "seed", "1"])

    def test_counters(self):
        text = "Inter-| Receive | Transmit\n lo: 999 7 0 0 0 0 0 0 1234 9 1 2 0 0 0 0\n"
        first = parse_lo_counters(text)
        self.assertEqual(first, dict(tx_bytes=1234, tx_packets=9, tx_errors=1, tx_dropped=2))
        last = dict(tx_bytes=1300, tx_packets=10, tx_errors=1, tx_dropped=2)
        self.assertEqual(counter_delta(first, last), dict(tx_bytes=66, tx_packets=1, tx_errors=0, tx_dropped=0))
        with self.assertRaises(TrialError):
            counter_delta(last, first)
        with self.assertRaises(TrialError):
            parse_lo_counters("lo: 1 2 3")

    def test_all_loss_still_offers_bytes(self):
        # Linux loopback probe: IPv6 UDP100 -> 162 clsact bytes, root netem loss100%.
        start = {"qdisc": [{"kind": "clsact", "bytes": 0, "packets": 0}]}
        end = {"qdisc": [{"kind": "clsact", "bytes": 162, "packets": 1}]}
        offered = pre_netem_delta(start, end)
        self.assertEqual(offered["accounted_bytes"], 162)
        self.assertEqual(offered["estimated_l3_bytes"], 148)
        dropped_lo = parse_lo_counters("lo: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0")
        self.assertEqual(dropped_lo["tx_bytes"], 0)
        self.assertGreater(offered["accounted_bytes"], dropped_lo["tx_bytes"])
        with self.assertRaises(TrialError):
            pre_netem_delta(end, start)

    def test_failure_classification(self):
        worker = {"status": "complete", "benchmark_returncode": 0}
        report = {"schema_version": 1, "status": "complete"}
        self.assertEqual(classify(0, False, worker, report), "complete")
        self.assertEqual(classify(-9, True, worker, report), "hard_timeout")
        self.assertEqual(classify(1, False, None, None), "isolation_launch_failed")
        self.assertEqual(classify(1, False, {"status": "failed", "failure_kind": "setup_failed"}, None), "setup_failed")
        self.assertEqual(classify(0, False, worker, None), "report_missing_or_invalid")
        self.assertEqual(classify(1, False, worker, dict(report, status="partial")), "benchmark_partial")
        self.assertEqual(classify(0, False, worker, dict(report, schema_version=2)), "unsupported_report_schema")

    def test_timeout_group_only(self):
        child = mock.Mock(pid=12345)
        child.wait.side_effect = [subprocess.TimeoutExpired("trial", 3), 0]
        with mock.patch("os.killpg") as kill:
            terminate_group(child)
        self.assertEqual(kill.call_args_list, [mock.call(12345, signal.SIGTERM), mock.call(12345, signal.SIGKILL)])

    def test_timeout_artifacts_and_report_preservation(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            directory = Path(temporary)
            raw = '{"schema_version":1,"status":"partial", "failure":"deadline"}\n'
            (directory / "report.json").write_text(raw)
            config = {"directory": temporary, "binary": "/fixture", "bench": {},
                      "tools": {"unshare": "/unshare"}, "hard_timeout_secs": 1,
                      "parent_namespace": {"link": "net:[123]", "inode": 123}}
            child = mock.Mock(pid=23456, returncode=-9)
            child.wait.side_effect = subprocess.TimeoutExpired("trial", 1)
            with mock.patch("subprocess.Popen", return_value=child) as popen, \
                    mock.patch(__name__ + ".terminate_group") as terminate:
                result = launch_trial(config)
            terminate.assert_called_once_with(child)
            self.assertTrue(popen.call_args.kwargs["start_new_session"])
            self.assertEqual(popen.call_args.kwargs["env"]["D28_BENCH_NETNS_PARENT"], "net:[123]")
            self.assertEqual(result["status"], "hard_timeout")
            self.assertTrue((directory / "result.json").exists())
            self.assertEqual((directory / "report.json").read_text(), raw)
            self.assertTrue((directory / "namespace.stderr.txt").exists())

    def test_matrix_and_defaults(self):
        args = self.parse("matrix", "--out", "new")
        validate_args(args)
        self.assertEqual(args.chatter_secs, 1)
        self.assertEqual(args.hard_timeout_secs, 201)
        self.assertIsNone(args.max_body)
        cases = cases_for(args)
        self.assertEqual(len(cases), 5)
        self.assertEqual(cases[-1]["offline"], 1)
        self.assertEqual(cases[-2]["rate_mbit"], 10)
        self.assertEqual(cases[2]["jitter_ms"], 5)

    def test_no_overwrite(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            path = Path(temporary) / "manifest.json"
            write_json(path, {"original": True})
            with self.assertRaises(FileExistsError):
                write_json(path, {"replacement": True})
            self.assertEqual(read_json(path), {"original": True})

    def test_summary_units_and_partial_trials(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            directory = Path(temporary)
            write_json(directory / "result.json", {"status": "benchmark_partial"})
            write_json(directory / "report.json", {
                "schema_version": 1, "summary": {
                    "latency_samples": {"first": 3, "quorum": 3, "all": 2},
                    "measured_heights": 2, "latency_ms": {"all": {"p50": 12, "p95": 14, "p99": 15},
                                                           "first": {"p50": 8}, "quorum": None},
                    "committed_body_bytes_per_second": 1000, "committed_payload_bytes_per_second": 800},
                "network": {"delta": {"tx_bytes": 9000, "tx_packets": 90},
                            "tc_delta": {"pre_impairment_accounted_bytes": 10400, "netem_drops": 10}},
                "chatter_network": {"delta": {"tx_bytes": 50}}})
            row = summary_row(directory)
            self.assertEqual(row["status"], "benchmark_partial")
            self.assertEqual(row["completed_n"], "2.000")
            self.assertEqual(row["first_n"], "3.000")
            self.assertEqual(row["quorum_n"], "3.000")
            self.assertEqual(row["all_n"], "2.000")
            self.assertEqual(row["all_p50_ms"], "12.000")
            self.assertEqual(row["first_p50_ms"], "8.000")
            self.assertEqual(row["quorum_p50_ms"], "-")
            self.assertEqual(row["body_B/s"], "1000.000")
            self.assertEqual(row["payload_B/s"], "800.000")
            self.assertEqual(row["measured_lo_TX_B"], "9000.000")
            self.assertEqual(row["measured_pre_L2_B"], "10400.000")
            self.assertEqual(row["measured_drops"], "10.000")
            self.assertEqual(row["tail_lo_TX_B"], "50.000")
            self.assertEqual(row["tail_pre_L2_B"], "-")

    def test_summary_keeps_missing_report(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            directory = Path(temporary)
            write_json(directory / "result.json", {"status": "isolation_launch_failed"})
            row = summary_row(directory)
            self.assertEqual(row["status"], "isolation_launch_failed")
            self.assertEqual(row["first_p95_ms"], "-")
            self.assertEqual(row["measured_lo_TX_B"], "-")


class NetnsTests(unittest.TestCase):
    """Opt-in prerequisite check with synthetic UDP, not a D28 performance result."""
    def test_live_clean_loss_and_timeout(self):
        fixture = '''import json, os, socket, sys, time
from pathlib import Path
report = Path(sys.argv[sys.argv.index("--json") + 1])
report.with_name("fixture-pid.json").write_text(json.dumps({"pid": os.getpid()}))
if sys.argv[sys.argv.index("--host-delay-ms") + 1] == "999":
    time.sleep(20)
with socket.socket(socket.AF_INET6, socket.SOCK_DGRAM) as receiver:
    receiver.bind(("::1", 0))
    with socket.socket(socket.AF_INET6, socket.SOCK_DGRAM) as sender:
        sender.sendto(b"x" * 100, receiver.getsockname())
        time.sleep(0.05)
report.write_text(json.dumps({"schema_version": 1, "status": "complete", "summary": {}}))
'''
        parent = namespace_identity()
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            root = Path(temporary)
            binary = root / "synthetic-bench"
            binary.write_text("#!" + sys.executable + "\n" + fixture)
            binary.chmod(0o700)
            for name, extra in (("clean", []), ("loss100", ["--loss-pct", "100"]),
                                ("timeout", ["--hard-timeout-secs", "1", "--host-delay-ms", "999"])):
                output = root / name
                args = parser().parse_args(["run", "--out", str(output), "--binary", str(binary),
                                            "--case", name, "--heights", "1", "--warmup", "0", *extra])
                validate_args(args)
                with contextlib.redirect_stdout(io.StringIO()):
                    code = run_trials(args)
                trial = output / (name + "-r001")
                result = read_json(trial / "result.json")
                worker = optional_json(trial / "worker.json")
                self.assertEqual(code, 1 if name == "timeout" else 0,
                                 f"{result}; {worker}; {(trial / 'namespace.stderr.txt').read_text()}")
                self.assertNotEqual(read_json(trial / "isolation.json")["child"], parent)
                self.assertEqual(worker["loopback_mtu"], 1500)
                if name == "timeout":
                    self.assertEqual(result["status"], "hard_timeout")
                    pid = read_json(trial / "fixture-pid.json")["pid"]
                    with self.assertRaises(ProcessLookupError):
                        os.kill(pid, 0)
                    continue
                self.assertEqual(worker["network_whole_process"]["pre_netem"]["accounted_bytes"], 162)
                self.assertEqual(worker["network_whole_process"]["pre_netem"]["estimated_l3_bytes"], 148)
                self.assertEqual(worker["network_whole_process"]["delta"]["tx_bytes"],
                                 0 if name == "loss100" else 148)
                self.assertEqual(worker["network_whole_process"]["netem_delta"]["drops"],
                                 1 if name == "loss100" else 0)
                self.assertTrue((trial / "tc-after-filter.stdout.txt").is_file())
        self.assertEqual(namespace_identity(), parent)


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if argv and argv[0] == "_trial":
        if len(argv) != 2:
            print("internal worker expects exactly one config path", file=sys.stderr)
            return 2
        return trial_worker(Path(argv[1]))
    argument_parser = parser()
    args = argument_parser.parse_args(argv)
    try:
        validate_args(args)
        if args.mode == "selftest":
            suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTests)
            if args.netns:
                suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(NetnsTests))
            return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
        if args.mode == "summary":
            summarize(args.directory)
            return 0
        return run_trials(args)
    except (OSError, ValueError, TrialError) as exc:
        print(f"d28_perf: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
