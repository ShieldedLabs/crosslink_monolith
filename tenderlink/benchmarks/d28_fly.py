#!/usr/bin/env python3
"""Private-6PN, stdlib-only D28 campaigns; never provision or alter host networking.

The campaign runs ON node 0, launching its fixed local benchmark and remote fixed-
binary workers. The TCP controller exchanges readiness/decision metadata only;
Rust alone obtains and validates bodies and certificates. All-online ACK pacing
adds a real control-network round trip. No clocks from different hosts subtract.

Quick contract (all output directories must be NEW):
  d28_fly.py token --out TOKEN                 # private random file, no token output
  d28_fly.py worker --bind '[fd00::2]:9200' --binary /app/d28_bench \
      --token-file TOKEN --root NEW
  d28_fly.py campaign --roster roster.json --binary /app/d28_bench \
      --token-file TOKEN --out NEW --seed 428
  d28_fly.py summary NEW
  d28_fly.py selftest

Roster: {"peers":["[fd00::1]:9000","[fd00::2]:9000",...],
         "workers":[null,"[fd00::2]:9200",...],
         "coordinator":"[fd00::1]:9100","interface":"eth0"}
The full roster includes offline nodes; online nodes are the leading N-offline.
Only literal ULA IPv6 addresses (or ::1 for tests) are accepted; no wildcard bind,
public listeners, shell commands, remote executable paths, installs or deployment.
"""

import argparse
import contextlib
import datetime
import hashlib
import hmac
import http.client
import http.server
import io
import ipaddress
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import secrets
import selectors
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock


MAX_BODY = 1048576
MAX_JSON = 8 * 1024 * 1024
MAX_LOG = 256 * 1024
MAX_LINE = 4096
PROFILES = {"baseline": {"poll_ms": 25, "status_ms": 100, "receive_budget": 32},
            "fast": {"poll_ms": 5, "status_ms": 25, "receive_budget": 128}}
RUN_ID = re.compile(r"[a-zA-Z0-9][a-zA-Z0-9_-]{15,95}\Z")
NOTES = [
    "One chain: body/payload bytes are counted once per all-online ACKed height, not once per validator.",
    "Controller receipt latency and cycle use controller monotonic time; node latency uses each node's own clock.",
    "All-online TCP ACK barrier is experimental network overhead, not a production protocol timer.",
    "TX is summed once across machines, never TX+RX. VM interface counters include TCP control, SSH and other activity.",
    "Whole-process interface counters include startup, warmup, tail and shutdown; do not divide by measured-chain bytes.",
    "Partial/failed trial durations remain in rate denominators, including failures with zero decisions.",
    "Percentiles pool observations (nearest rank); small samples cannot establish stable p95/p99 tails.",
    "Shared-CPU credits/noisy neighbours are uncontrolled; seeded matched repetitions randomize size/profile order.",
]


class ProtocolError(ValueError):
    pass


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def encode(value):
    return json.dumps(value, separators=(",", ":"), allow_nan=False).encode("utf-8")


def decode(raw):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError("duplicate JSON key")
            result[key] = value
        return result
    return json.loads(raw, object_pairs_hook=pairs,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite JSON")))


def write_json(path, value):
    with Path(path).open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True, allow_nan=False)
        stream.write("\n")


def read_json(path):
    with Path(path).open("rb") as stream:
        raw = stream.read(MAX_JSON + 1)
    if len(raw) > MAX_JSON:
        raise ValueError("JSON artifact exceeds size limit")
    return decode(raw)


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def integer(value, low, high, name):
    if type(value) is not int or not low <= value <= high:
        raise ValueError(f"{name} must be integer in [{low}, {high}]")
    return value


def number(value, low, high, name):
    if type(value) not in (int, float) or not math.isfinite(value) or not low <= value <= high:
        raise ValueError(f"{name} must be finite in [{low}, {high}]")
    return value


def endpoint(value):
    if not isinstance(value, str):
        raise ValueError("endpoint must be [private IPv6]:port")
    match = re.fullmatch(r"\[([0-9a-fA-F:]+)\]:([0-9]{1,5})", value)
    if not match:
        raise ValueError("endpoint must be [private IPv6]:port")
    address = ipaddress.IPv6Address(match[1])
    if not (address in ipaddress.IPv6Network("fc00::/7") or address == ipaddress.IPv6Address("::1")):
        raise ValueError("only ULA IPv6 or ::1 endpoints allowed; never wildcard/public")
    return str(address), integer(int(match[2]), 1024, 65535, "port")


def run_id(value):
    if not isinstance(value, str) or RUN_ID.fullmatch(value) is None:
        raise ValueError("run_id must contain 16..96 safe ASCII characters")
    return value


def validate_config(config):
    if not isinstance(config, dict):
        raise ValueError("config must be an object")
    bounds = {"node": (0, 31), "nodes": (1, 32), "offline": (0, 31),
              "headers": (0, 2048), "solution_bytes": (0, 1344), "heights": (1, 200),
              "warmup": (0, 20), "timeout_secs": (1, 1800), "workers": (1, 1),
              "max_body": (MAX_BODY, MAX_BODY), "chatter_secs": (0, 30),
              "poll_ms": (5, 25), "status_ms": (25, 100), "receive_budget": (32, 128),
              "max_requests": (2, 2), "max_outgoing": (2, 2)}
    required = set(bounds) - {"solution_bytes"}
    if set(config) - (set(bounds) | {"peers", "coordinator", "run_id"}) or not required <= set(config):
        raise ValueError("unknown or missing config fields")
    for key, (low, high) in bounds.items():
        integer(config.get(key, 0), low, high, key)
    if config["node"] >= config["nodes"] - config["offline"]:
        raise ValueError("node must be online")
    if 3 * (config["nodes"] - config["offline"]) <= 2 * config["nodes"]:
        raise ValueError("online nodes must retain >2/3 quorum")
    if {key: config[key] for key in PROFILES["baseline"]} not in PROFILES.values():
        raise ValueError("only baseline or fast host-service profile allowed")
    peers = config.get("peers")
    if not isinstance(peers, list) or len(peers) != config["nodes"]:
        raise ValueError("peers must be the full ordered roster")
    addresses = [endpoint(peer) for peer in peers]
    if len(set(addresses)) != len(addresses):
        raise ValueError("peer endpoints must be unique")
    if endpoint(config.get("coordinator")) in addresses:
        raise ValueError("coordinator must have a distinct endpoint")
    run_id(config.get("run_id"))
    return dict(config)


def percentile(values):
    ordered = sorted(values)
    return None if not ordered else {f"p{p}": ordered[max(0, math.ceil(len(ordered) * p / 100) - 1)]
                                    for p in (50, 95, 99)}


class Barrier:
    """Pure agreement state; only metadata, never a body or certificate oracle."""
    def __init__(self, config, now):
        self.config = validate_config(config)
        self.online = config["nodes"] - config["offline"]
        self.quorum = 2 * config["nodes"] // 3 + 1
        self.created = now
        self.ready_nodes = {}
        self.started = None
        self.anchor = None
        self.measured_start = None
        self.finished = None
        self.failure = None
        self.rows = []
        self.height = 1
        self.complete = False

    def ready(self, message, now):
        if set(message) != {"type", "run_id", "node"} or message.get("type") != "ready":
            raise ProtocolError("expected ready")
        self.identity(message)
        node = message["node"]
        if self.started is not None or node in self.ready_nodes:
            raise ProtocolError("duplicate or late ready")
        self.ready_nodes[node] = now - self.created
        return len(self.ready_nodes) == self.online

    def identity(self, message):
        if message.get("run_id") != self.config["run_id"]:
            raise ProtocolError("wrong run_id")
        integer(message.get("node"), 0, self.online - 1, "node")

    def start(self, now):
        if len(self.ready_nodes) != self.online or self.started is not None:
            raise ProtocolError("start before every online node ready or duplicate start")
        self.started = self.anchor = now
        if self.config["warmup"] == 0:
            self.measured_start = now

    def decision(self, message, now):
        fields = {"type", "run_id", "node", "height", "id", "body_bytes", "payload_bytes", "latency_ms", "round"}
        if set(message) != fields or message.get("type") != "decision":
            raise ProtocolError("invalid decision fields")
        self.identity(message)
        node = message["node"]
        if self.started is None or self.complete or node not in self.ready_nodes:
            raise ProtocolError("decision before ready/start or after completion")
        integer(message["height"], 1, self.config["heights"] + self.config["warmup"], "height")
        if message["height"] != self.height:
            raise ProtocolError("out-of-order height")
        if not isinstance(message["id"], str) or re.fullmatch(r"[0-9a-f]{64}", message["id"]) is None:
            raise ProtocolError("invalid body ID")
        integer(message["body_bytes"], 1, MAX_BODY, "body_bytes")
        integer(message["payload_bytes"], 0, message["body_bytes"], "payload_bytes")
        integer(message["round"], 0, 2**31 - 1, "round")
        number(message["latency_ms"], 0, self.config["timeout_secs"] * 1000 + 1000, "latency_ms")
        if len(self.rows) < self.height:
            self.rows.append({"height": self.height, "measured": self.height > self.config["warmup"],
                              "id": message["id"], "body_bytes": message["body_bytes"],
                              "payload_bytes": message["payload_bytes"], "decisions": [],
                              "acknowledged": False})
        row = self.rows[-1]
        if any(item["node"] == node for item in row["decisions"]):
            raise ProtocolError("duplicate decision")
        if any(row[key] != message[key] for key in ("id", "body_bytes", "payload_bytes")):
            raise ProtocolError("conflicting body ID or byte counts")
        row["decisions"].append({"node": node, "node_latency_ms": message["latency_ms"],
                                 "round": message["round"], "receipt_ms": (now - self.anchor) * 1000})
        return len(row["decisions"]) == self.online

    def acked(self, began, ended):
        row = self.rows[-1]
        if len(row["decisions"]) != self.online or row["acknowledged"]:
            raise ProtocolError("ACK without unique all-online agreement")
        row.update(acknowledged=True, ack_send_ms=(ended - began) * 1000,
                   ack_begin_receipt_ms=(began - self.anchor) * 1000,
                   cycle_ms=(began - self.anchor) * 1000)
        # The NEXT cycle includes previous ACK transmission/network delivery.
        self.anchor = began
        if self.height == self.config["warmup"]:
            self.measured_start = began
        self.height += 1
        if self.height > self.config["heights"] + self.config["warmup"]:
            self.complete = True
            self.finished = ended

    def finish(self, now, failure=None):
        if self.finished is None:
            self.finished = now
        if failure:
            self.failure = failure
            self.complete = False

    def report(self, now):
        end = self.finished if self.finished is not None else now
        # A failed startup/warmup is not silently removed from campaign rates.
        rate_start = self.measured_start if self.measured_start is not None else self.created
        return {"schema_version": 1, "run_id": self.config["run_id"],
                "status": "complete" if self.complete else "partial", "failure": self.failure,
                "config": self.config, "ready_nodes": self.ready_nodes, "heights": self.rows,
                "elapsed_seconds": max(0, end - self.created),
                "rate_duration_seconds": max(0, end - rate_start),
                "rate_duration_scope": ("measured start through final ACK/failure" if self.measured_start is not None
                                        else "failed before measured start: entire controller lifetime"),
                "notes": NOTES}


class Coordinator:
    def __init__(self, config, ready_timeout=30):
        self.config = config
        self.state = Barrier(config, time.monotonic())
        self.ready_timeout = ready_timeout
        self.done = threading.Event()
        self.stop = threading.Event()
        self.error = None
        self.rx_bytes = self.tx_bytes = 0
        self.listener = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
        try:
            self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            self.listener.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            self.listener.bind(endpoint(config["coordinator"]))
            self.listener.listen(config["nodes"])
            self.listener.setblocking(False)
        except BaseException:
            self.listener.close()
            raise
        self.thread = threading.Thread(target=self.run, name="d28-coordinator", daemon=True)

    def launch(self):
        self.thread.start()

    def abort(self, reason):
        self.error = reason
        self.stop.set()

    def send(self, clients, message):
        raw = encode(message) + b"\n"
        for client in clients:
            client.sendall(raw)
            self.tx_bytes += len(raw)

    def run(self):
        clients = {}
        by_node = {}
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(self.listener, selectors.EVENT_READ)
                while not self.state.complete:
                    now = time.monotonic()
                    if self.stop.is_set():
                        raise ProtocolError(self.error or "aborted")
                    deadline = ((self.state.started + self.config["timeout_secs"]) if self.state.started is not None
                                else self.state.created + self.ready_timeout)
                    if now >= deadline:
                        raise ProtocolError("decision timeout" if self.state.started else "ready timeout")
                    for key, _ in selector.select(min(0.1, deadline - now)):
                        client = key.fileobj
                        if client is self.listener:
                            child, address = self.listener.accept()
                            if len(clients) >= self.state.online:
                                child.close()
                                raise ProtocolError("too many coordinator connections")
                            child.settimeout(2)
                            child.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                            clients[child] = {"buffer": b"", "node": None, "host": str(ipaddress.IPv6Address(address[0]))}
                            selector.register(child, selectors.EVENT_READ)
                            continue
                        connection = clients[client]
                        raw = client.recv(MAX_LINE + 1)
                        if not raw:
                            raise ProtocolError("node disconnected before final ACK")
                        self.rx_bytes += len(raw)
                        connection["buffer"] += raw
                        if len(connection["buffer"]) > MAX_LINE:
                            raise ProtocolError("coordinator line limit exceeded")
                        while b"\n" in connection["buffer"]:
                            line, connection["buffer"] = connection["buffer"].split(b"\n", 1)
                            message = decode(line)
                            if not isinstance(message, dict):
                                raise ProtocolError("message must be an object")
                            received = time.monotonic()
                            if connection["node"] is None:
                                all_ready = self.state.ready(message, received)
                                node = message["node"]
                                if endpoint(self.config["peers"][node])[0] != connection["host"]:
                                    raise ProtocolError("ready source address does not match peer roster")
                                connection["node"] = node
                                by_node[node] = client
                                if all_ready:
                                    self.state.start(time.monotonic())
                                    self.send([by_node[n] for n in range(self.state.online)], {"type": "start"})
                            else:
                                if message.get("node") != connection["node"]:
                                    raise ProtocolError("connection node identity changed")
                                if self.state.decision(message, received):
                                    began = time.monotonic()
                                    self.send([by_node[n] for n in range(self.state.online)],
                                              {"type": "ack", "height": self.state.height})
                                    self.state.acked(began, time.monotonic())
        except (OSError, ValueError) as exc:
            self.state.finish(time.monotonic(), f"{type(exc).__name__}: {exc}")
        finally:
            for client in clients:
                client.close()
            self.listener.close()
            self.state.finish(time.monotonic())
            self.done.set()

    def report(self):
        report = self.state.report(time.monotonic())
        report["control_json_bytes"] = {"rx": self.rx_bytes, "tx": self.tx_bytes,
                                        "scope": "controller JSON payload only, not TCP/IP bytes"}
        return report


def aggregate(trials):
    groups = {}
    for trial in trials:
        config = trial["config"]
        key = (config["nodes"], config["offline"], config["headers"], config.get("solution_bytes", 0), trial["profile"])
        group = groups.setdefault(key, {"nodes": key[0], "offline": key[1], "headers": key[2],
                                       "solution_bytes": key[3], "profile": key[4], "trials": 0,
                                       "complete_trials": 0, "duration_seconds": 0, "completed_heights": 0,
                                       "body_bytes": 0, "payload_bytes": 0, "body_sizes": set(),
                                       "receipt_ms": {c: [] for c in ("first", "quorum", "all")},
                                       "node_local_ms": [], "cycle_ms": [], "ack_send_ms": []})
        group["trials"] += 1
        group["complete_trials"] += trial["status"] == "complete"
        controller = trial["controller"]
        group["duration_seconds"] += controller["rate_duration_seconds"]
        for row in controller["heights"]:
            if not row["measured"]:
                continue
            receipts = sorted(item["receipt_ms"] for item in row["decisions"])
            group["node_local_ms"].extend(item["node_latency_ms"] for item in row["decisions"])
            for cohort, count in (("first", 1), ("quorum", 2 * key[0] // 3 + 1), ("all", key[0] - key[1])):
                if len(receipts) >= count:
                    group["receipt_ms"][cohort].append(receipts[count - 1])
            if row["acknowledged"]:
                group["completed_heights"] += 1
                for field in ("body_bytes", "payload_bytes"):
                    group[field] += row[field]
                group["body_sizes"].add(row["body_bytes"])
                group["cycle_ms"].append(row["cycle_ms"])
                group["ack_send_ms"].append(row["ack_send_ms"])
    result = []
    for key in sorted(groups):
        group = groups[key]
        group["body_sizes"] = sorted(group["body_sizes"])
        for field in ("completed_heights", "body_bytes", "payload_bytes"):
            group[field + "_per_second"] = (group[field] / group["duration_seconds"]
                                              if group["duration_seconds"] > 0 else None)
        for field in ("node_local_ms", "cycle_ms", "ack_send_ms"):
            values = group[field]
            group[field] = {"n": len(values), "percentiles": percentile(values)}
        group["receipt_ms"] = {cohort: {"n": len(values), "percentiles": percentile(values)}
                                for cohort, values in group["receipt_ms"].items()}
        result.append(group)
    return result


def read_token(path):
    # Refuse symlinks and group/world-readable files. Never print/persist token.
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(descriptor, "r", encoding="ascii") as stream:
        metadata = os.fstat(stream.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o077 or metadata.st_uid != os.getuid():
            raise ValueError("token must be an owned regular private (0600) file")
        token = stream.read(257).strip()
    if re.fullmatch(r"[0-9a-f]{64}", token) is None:
        raise ValueError("token must contain exactly 64 random hexadecimal characters")
    return token


def create_token(path):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="ascii") as stream:
        stream.write(secrets.token_hex(32) + "\n")


def optional_text(path, limit=65536):
    try:
        with Path(path).open(encoding="utf-8", errors="replace") as stream:
            return stream.read(limit)
    except OSError:
        return None


def machine_metadata(binary):
    cpuinfo = optional_text("/proc/cpuinfo") or ""
    cpu = next((line.split(":", 1)[1].strip() for line in cpuinfo.splitlines()
                if line.startswith(("model name", "Hardware")) and ":" in line), None)
    return {"created_at": utc_now(), "binary_sha256": sha256(binary),
            "binary_bytes": binary.stat().st_size, "binary_path": str(binary),
            "kernel": platform.uname()._asdict(), "python": sys.version,
            "cpu_model": cpu, "logical_cpus": os.cpu_count(),
            "affinity": sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
            "load_average": list(os.getloadavg()),
            "fly": {key: os.environ.get(key) for key in
                    ("FLY_APP_NAME", "FLY_MACHINE_ID", "FLY_REGION", "PRIMARY_REGION", "FLY_VM_MEMORY_MB")},
            "memory_info": optional_text("/proc/meminfo"),
            "cgroup_cpu_max": optional_text("/sys/fs/cgroup/cpu.max"),
            "cgroup_memory_max": optional_text("/sys/fs/cgroup/memory.max"),
            "build_recipe_note": "Actual executable hash recorded; no claim about an unobserved build command."}


def system_snapshot(interface):
    began = time.monotonic()
    network = None
    for line in (optional_text("/proc/net/dev") or "").splitlines():
        name, separator, text = line.partition(":")
        if separator and name.strip() == interface:
            values = text.split()
            if len(values) == 16:
                network = {"rx_bytes": int(values[0]), "rx_packets": int(values[1]),
                           "tx_bytes": int(values[8]), "tx_packets": int(values[9])}
    cpu = None
    for line in (optional_text("/proc/stat") or "").splitlines():
        if line.startswith("cpu "):
            values = [int(value) for value in line.split()[1:]]
            cpu = dict(zip(("user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal",
                            "guest", "guest_nice"), values))
            break
    cgroup = None
    text = optional_text("/sys/fs/cgroup/cpu.stat")
    if text is not None:
        cgroup = {}
        for line in text.splitlines():
            fields = line.split()
            if len(fields) == 2 and fields[1].isdigit():
                cgroup[fields[0]] = int(fields[1])
    return {"monotonic_seconds": began, "sample_seconds": time.monotonic() - began,
            "interface": interface, "network": network, "cpu_jiffies": cpu, "cgroup_cpu_stat": cgroup}


def deltas(before, after):
    if before is None or after is None or before.keys() != after.keys():
        return None
    if any(type(before[key]) is not int or type(after[key]) is not int or after[key] < before[key] for key in before):
        return None
    return {key: after[key] - before[key] for key in before}


def stop_child(child):
    """Only the process group of the specific Popen(start_new_session=True)."""
    if child.poll() is not None:
        return
    with contextlib.suppress(ProcessLookupError):
        os.killpg(child.pid, signal.SIGTERM)
    try:
        child.wait(timeout=2)
    except subprocess.TimeoutExpired:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(child.pid, signal.SIGKILL)
        child.wait(timeout=3)


class NodeJob:
    def __init__(self, binary, expected_sha, directory, config, interface):
        self.directory = directory
        self.config = validate_config(config)
        self.done = threading.Event()
        self.cancelled = False
        self.result = None
        self.child = None
        if sha256(binary) != expected_sha:
            raise ValueError("binary changed since manifest")
        directory.mkdir(mode=0o700)
        write_json(directory / "config.json", config)
        self.metadata = machine_metadata(binary)
        write_json(directory / "manifest.json", self.metadata)
        self.command = [str(binary), "--distributed", str(directory / "config.json"),
                        "--json", str(directory / "report.json")]
        write_json(directory / "argv.json", self.command)
        self.began = time.monotonic()
        self.before = system_snapshot(interface)
        self.interface = interface
        self.log_counts = {}
        self.log_threads = []
        self.watcher = None
        try:
            self.child = subprocess.Popen(self.command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                          stdin=subprocess.DEVNULL, start_new_session=True)
            for label, source in (("stdout", self.child.stdout), ("stderr", self.child.stderr)):
                thread = threading.Thread(target=self.capture, args=(label, source), daemon=True)
                # Track before start: an interrupt can arrive after the thread starts.
                self.log_threads.append(thread)
                thread.start()
            self.watcher = threading.Thread(target=self.watch, name="d28-child-watch", daemon=True)
            self.watcher.start()
        except BaseException as exc:
            # The caller cannot register this job until construction returns.
            # Thus every post-Popen failure must retire this exact owned child here.
            if self.child is not None:
                with contextlib.suppress(OSError, subprocess.TimeoutExpired):
                    stop_child(self.child)
                for thread in [*self.log_threads, self.watcher]:
                    if thread is not None and thread.ident is not None:
                        with contextlib.suppress(RuntimeError):
                            thread.join(timeout=3)
                for source in (self.child.stdout, self.child.stderr):
                    if source is not None:
                        with contextlib.suppress(OSError):
                            source.close()
            if isinstance(exc, OSError):
                if not self.done.is_set():
                    self.finish(None, False, str(exc))
            else:
                raise

    def capture(self, label, source):
        count = 0
        try:
            with source, (self.directory / (label + ".txt")).open("xb") as output:
                while True:
                    chunk = source.read(65536)
                    if not chunk:
                        break
                    output.write(chunk[:max(0, MAX_LOG - count)])
                    count += len(chunk)
        finally:
            self.log_counts[label] = {"total_bytes": count, "retained_limit": MAX_LOG,
                                      "truncated": count > MAX_LOG}

    def watch(self):
        timed_out = False
        error = None
        try:
            self.child.wait(timeout=self.config["timeout_secs"] + self.config["chatter_secs"] + 45)
        except subprocess.TimeoutExpired:
            timed_out = True
            stop_child(self.child)
        except OSError as exc:
            error = str(exc)
            stop_child(self.child)
        for thread in self.log_threads:
            thread.join(timeout=3)
        self.finish(self.child.returncode, timed_out, error)

    def finish(self, returncode, timed_out, error):
        after = system_snapshot(self.interface)
        self.result = {"run_id": self.config["run_id"], "node": self.config["node"],
                       "returncode": returncode, "timed_out": timed_out,
                       "cancelled": self.cancelled, "error": error,
                       "elapsed_seconds": time.monotonic() - self.began,
                       "binary_sha256": self.metadata["binary_sha256"], "logs": self.log_counts,
                       "process_window": {"start": self.before, "end": after,
                                          "network_delta": deltas(self.before["network"], after["network"]),
                                          "cpu_jiffies_delta": deltas(self.before["cpu_jiffies"], after["cpu_jiffies"]),
                                          "cgroup_cpu_delta": deltas(self.before["cgroup_cpu_stat"], after["cgroup_cpu_stat"])}}
        self.result["process_window"]["scope"] = "whole child incl startup/warmup/tail/shutdown; counters include control and unrelated VM work"
        try:
            write_json(self.directory / "worker.json", self.result)
        except OSError as exc:
            self.result["error"] = str(exc)
        finally:
            self.done.set()

    def cancel(self):
        if not self.done.is_set():
            self.cancelled = True
            if self.child is not None:
                stop_child(self.child)
            self.done.wait(6)

    def artifacts(self):
        if not self.done.is_set():
            raise ValueError("result not ready")
        files = {}
        for name in ("config.json", "manifest.json", "argv.json", "worker.json", "report.json", "stdout.txt", "stderr.txt"):
            path = self.directory / name
            if path.is_file():
                with path.open("rb") as stream:
                    raw = stream.read(MAX_JSON + 1)
                if len(raw) > MAX_JSON:
                    raise ValueError("artifact size limit exceeded")
                files[name] = raw.decode("utf-8", errors="replace")
        return {"run_id": self.config["run_id"], "node": self.config["node"], "files": files}


class Worker:
    def __init__(self, binary, root, interface, max_runs=128):
        self.binary = binary.resolve(strict=True)
        if not self.binary.is_file() or not os.access(self.binary, os.X_OK):
            raise ValueError("binary must be an executable regular file")
        self.metadata = machine_metadata(self.binary)
        self.root = root.resolve()
        self.root.mkdir(mode=0o700, parents=True)
        write_json(self.root / "manifest.json", self.metadata)
        self.interface = interface
        self.jobs = {}
        self.max_runs = max_runs

    def start(self, config):
        config = validate_config(config)
        identifier = config["run_id"]
        if identifier in self.jobs:
            raise ValueError("run_id already used; never relaunch a timed-out HTTP request")
        if len(self.jobs) >= self.max_runs:
            raise ValueError("worker run limit reached")
        if any(not job.done.is_set() for job in self.jobs.values()):
            raise ValueError("worker already has a running child")
        job = NodeJob(self.binary, self.metadata["binary_sha256"], self.root / identifier, config, self.interface)
        self.jobs[identifier] = job
        return {"run_id": identifier, "node": config["node"], "accepted": True}

    def stop_all(self):
        for job in self.jobs.values():
            job.cancel()


class WorkerServer(http.server.HTTPServer):
    address_family = socket.AF_INET6
    request_queue_size = 4

    def server_bind(self):
        self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        super().server_bind()

    def get_request(self):
        child, address = super().get_request()
        child.settimeout(3)
        return child, address


class WorkerHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    server_version = "D28PrivateWorker/1"

    def log_message(self, *_args):
        pass  # No request/token/config logging.

    def respond(self, status, value):
        raw = encode(value)
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def dispatch(self):
        try:
            auth = self.headers.get_all("Authorization", [])
            expected = "Bearer " + self.server.token
            if len(auth) != 1 or not hmac.compare_digest(auth[0].encode("utf-8"), expected.encode("ascii")):
                self.respond(403, {"error": "unauthorized"})
                return
            worker = self.server.worker
            if self.command == "GET" and self.path == "/v1/info":
                self.respond(200, worker.metadata)
                return
            if self.command != "POST":
                self.respond(404, {"error": "no such operation"})
                return
            lengths = self.headers.get_all("Content-Length", [])
            if len(lengths) != 1 or self.headers.get("Transfer-Encoding") or not lengths[0].isdigit():
                raise ValueError("one Content-Length required; no transfer encoding")
            length = integer(int(lengths[0]), 1, 16384, "request size")
            raw = self.rfile.read(length)
            if len(raw) != length:
                raise ValueError("short request")
            request = decode(raw)
            if self.path == "/v1/start":
                value = worker.start(request)
            elif self.path in ("/v1/status", "/v1/result", "/v1/stop"):
                if not isinstance(request, dict) or set(request) != {"run_id"}:
                    raise ValueError("expected run_id only")
                job = worker.jobs[run_id(request["run_id"])]
                if self.path == "/v1/stop":
                    job.cancel()
                value = (job.artifacts() if self.path == "/v1/result" else
                         {"run_id": request["run_id"], "done": job.done.is_set(), "result": job.result})
            else:
                self.respond(404, {"error": "no such operation"})
                return
            self.respond(200, value)
        except (ValueError, OSError, KeyError, TypeError) as exc:
            # Do not echo arbitrary input, auth headers or configuration contents.
            with contextlib.suppress(OSError):
                self.respond(400, {"error": type(exc).__name__, "detail": "request rejected; check bounded config/state"})

    do_GET = dispatch
    do_POST = dispatch


def worker_request(address, token, operation, value=None, timeout=10):
    host, port = endpoint(address)
    connection = http.client.HTTPConnection(host, port, timeout=timeout)
    try:
        headers = {"Authorization": "Bearer " + token, "Content-Type": "application/json"}
        connection.request("GET" if value is None else "POST", "/v1/" + operation,
                           body=None if value is None else encode(value), headers=headers)
        response = connection.getresponse()
        raw = response.read(MAX_JSON + 1)
        if len(raw) > MAX_JSON:
            raise ValueError("worker response size limit exceeded")
        result = decode(raw)
        if response.status != 200:
            raise ValueError(f"worker {operation} HTTP {response.status}: rejected")
        return result
    finally:
        connection.close()


def serve_worker(args):
    token = read_token(args.token_file)
    worker = Worker(args.binary, args.root, args.interface, args.max_runs)
    deadline = time.monotonic() + args.lifetime_secs
    try:
        with WorkerServer(endpoint(args.bind), WorkerHandler) as server:
            server.worker = worker
            server.token = token
            server.timeout = 0.25
            print(f"Private fixed-binary worker listening on {args.bind}; lifetime {args.lifetime_secs}s", flush=True)
            while time.monotonic() < deadline:
                server.handle_request()
    finally:
        worker.stop_all()
    return 0


def validate_roster(roster):
    if not isinstance(roster, dict) or set(roster) - {"peers", "workers", "coordinator", "interface"}:
        raise ValueError("roster fields: peers, workers, coordinator, optional interface")
    peers, workers = roster.get("peers"), roster.get("workers")
    if not isinstance(peers, list) or not 1 <= len(peers) <= 32:
        raise ValueError("roster must contain 1..32 peers")
    if not isinstance(workers, list) or len(workers) != len(peers) or workers[0] is not None:
        raise ValueError("workers must match peers; workers[0] MUST be null (direct local child)")
    for index, worker in enumerate(workers[1:], 1):
        if endpoint(worker)[0] != endpoint(peers[index])[0]:
            raise ValueError("worker and peer must name the same private host")
    if endpoint(roster.get("coordinator"))[0] != endpoint(peers[0])[0]:
        raise ValueError("coordinator must be on node0")
    addresses = [endpoint(peer) for peer in peers] + [endpoint(worker) for worker in workers[1:]]
    addresses.append(endpoint(roster["coordinator"]))
    if len(set(addresses)) != len(addresses):
        raise ValueError("peer, worker and coordinator endpoints must be distinct")
    interface = roster.get("interface", "eth0")
    if not isinstance(interface, str) or re.fullmatch(r"[a-zA-Z0-9_.-]{1,15}", interface) is None:
        raise ValueError("invalid interface name")
    return dict(roster, interface=interface)


def schedule(headers, profiles, repetitions, seed):
    rng = random.Random(seed)
    trials = []
    for repetition in range(1, repetitions + 1):
        sizes = list(headers)
        rng.shuffle(sizes)
        for size in sizes:
            paired = list(profiles)
            rng.shuffle(paired)
            for profile in paired:
                trials.append({"repetition": repetition, "headers": size, "profile": profile})
    return trials


def collect_remote(address, token, identifier, directory, grace):
    deadline = time.monotonic() + grace
    request = {"run_id": identifier}
    while True:
        status = worker_request(address, token, "status", request, timeout=5)
        if status.get("done"):
            break
        if time.monotonic() >= deadline:
            worker_request(address, token, "stop", request, timeout=10)
            break
        time.sleep(0.2)  # Only AFTER final ACK/failure, never during measured heights.
    artifacts = worker_request(address, token, "result", request, timeout=10)
    if artifacts.get("run_id") != identifier or not isinstance(artifacts.get("files"), dict):
        raise ValueError("invalid worker result identity")
    names = {"config.json", "manifest.json", "argv.json", "worker.json", "report.json", "stdout.txt", "stderr.txt"}
    if set(artifacts["files"]) - names:
        raise ValueError("unexpected worker artifact")
    for name, raw in artifacts["files"].items():
        if not isinstance(raw, str) or len(raw.encode("utf-8")) > MAX_JSON:
            raise ValueError("invalid worker artifact text")
        with (directory / name).open("x", encoding="utf-8") as stream:
            stream.write(raw)


def validate_node_artifacts(directory, config, expected_sha):
    worker = read_json(directory / "worker.json")
    report = read_json(directory / "report.json")
    manifest = read_json(directory / "manifest.json")
    actual = read_json(directory / "config.json")
    if actual != config or manifest.get("binary_sha256") != expected_sha or worker.get("binary_sha256") != expected_sha:
        raise ValueError("node config/binary provenance mismatch")
    if worker.get("run_id") != config["run_id"] or worker.get("node") != config["node"]:
        raise ValueError("node result identity mismatch")
    if worker.get("returncode") != 0 or worker.get("timed_out") or worker.get("cancelled") or worker.get("error"):
        raise ValueError("node process did not exit successfully")
    if report.get("schema_version") != 1 or report.get("mode") != "distributed" or report.get("status") != "complete":
        raise ValueError("node report missing, partial or unsupported")
    if report.get("config", {}).get("run_id") != config["run_id"] or report.get("config", {}).get("node") != config["node"]:
        raise ValueError("node report identity mismatch")
    return worker, report


def run_trial(args, roster, token, binary, metadata, directory, case, campaign_deadline):
    directory.mkdir(mode=0o700)
    began = time.monotonic()
    config = {"node": 0, "nodes": len(roster["peers"]), "offline": args.offline,
              "headers": case["headers"], "solution_bytes": args.solution_bytes,
              "heights": args.heights, "warmup": args.warmup, "timeout_secs": args.timeout_secs,
              "workers": 1, "max_body": MAX_BODY, "chatter_secs": args.chatter_secs,
              "peers": roster["peers"], "coordinator": roster["coordinator"],
              "run_id": secrets.token_hex(24), "max_requests": 2, "max_outgoing": 2,
              **PROFILES[case["profile"]]}
    validate_config(config)
    write_json(directory / "trial.json", {"config": config, **case})
    local = controller = None
    attempted = []
    errors = []
    interrupted = False
    try:
        controller = Coordinator(config, args.ready_timeout_secs)
        controller.launch()
        local = NodeJob(binary, metadata["binary_sha256"], directory / "node-000", config, roster["interface"])
        for node in range(1, config["nodes"] - config["offline"]):
            remote_directory = directory / f"node-{node:03d}"
            remote_directory.mkdir(mode=0o700)
            remote_config = dict(config, node=node)
            write_json(remote_directory / "config.sent.json", remote_config)
            attempted.append(node)  # A timed-out start may have launched: stop only this run_id.
            response = worker_request(roster["workers"][node], token, "start", remote_config)
            if response.get("run_id") != config["run_id"] or response.get("node") != node or not response.get("accepted"):
                raise ValueError("worker launch response mismatch")
        available = max(0, min(campaign_deadline - time.monotonic(),
                               args.ready_timeout_secs + args.timeout_secs + 5))
        if not controller.done.wait(available):
            controller.abort("campaign/controller deadline")
        controller.thread.join(5)
        if not controller.done.is_set():
            raise ValueError("coordinator failed to stop")
    except KeyboardInterrupt:
        interrupted = True
        errors.append("interrupted")
    except (OSError, ValueError, http.client.HTTPException) as exc:
        errors.append(f"launch/controller: {type(exc).__name__}: {exc}")
    finally:
        if controller is not None and not controller.done.is_set():
            controller.abort(errors[-1] if errors else "controller stop")
            controller.thread.join(5)
        good_controller = controller is not None and controller.done.is_set() and controller.state.complete
        if not good_controller or errors:
            if local is not None:
                local.cancel()
            for node in attempted:
                try:
                    worker_request(roster["workers"][node], token, "stop", {"run_id": config["run_id"]})
                except (OSError, ValueError, http.client.HTTPException) as exc:
                    errors.append(f"node {node} stop: {type(exc).__name__}")
        # Collection begins only after the coordinator's final ACK or failure.
        cleanup_unconfirmed = []
        grace = args.chatter_secs + 8
        if local is not None and not local.done.wait(grace):
            local.cancel()
            errors.append("node 0 exit timeout")
        if local is not None and not local.done.is_set():
            cleanup_unconfirmed.append(0)
        for node in attempted:
            try:
                collect_remote(roster["workers"][node], token, config["run_id"],
                               directory / f"node-{node:03d}", grace)
            except (OSError, ValueError, http.client.HTTPException) as exc:
                errors.append(f"node {node} collection: {type(exc).__name__}: {exc}")
                # Even a successful final ACK does not prove the remote process
                # exited. Stop only this run, then require explicit done evidence.
                confirmed = False
                for operation in ("stop", "status"):
                    try:
                        reply = worker_request(roster["workers"][node], token, operation,
                                               {"run_id": config["run_id"]})
                        if (isinstance(reply, dict) and reply.get("run_id") == config["run_id"]
                                and reply.get("done") is True):
                            confirmed = True
                            break
                    except (OSError, ValueError, http.client.HTTPException) as cleanup_error:
                        errors.append(f"node {node} cleanup {operation}: {type(cleanup_error).__name__}")
                if not confirmed:
                    cleanup_unconfirmed.append(node)
                    errors.append(f"node {node} cleanup unconfirmed; campaign must halt")
    if controller is not None and controller.done.is_set():
        control_report = controller.report()
    else:
        failed = Barrier(config, began)
        failed.finish(time.monotonic(), errors[-1] if errors else "no controller report")
        control_report = failed.report(time.monotonic())
    write_json(directory / "controller.json", control_report)
    node_results = []
    for node in range(config["nodes"] - config["offline"]):
        node_directory = directory / f"node-{node:03d}"
        try:
            worker, report = validate_node_artifacts(node_directory, dict(config, node=node), metadata["binary_sha256"])
            # Final reports must describe the exact IDs and sizes ACKed by the controller.
            decisions = {row["height"]: row for row in report["heights"]}
            if len(decisions) != config["heights"] + config["warmup"]:
                raise ValueError("node final report height coverage mismatch")
            for row in control_report["heights"]:
                actual = decisions[row["height"]]
                if any(actual.get(key) != row[key] for key in ("id", "body_bytes", "payload_bytes")) or not actual.get("acknowledged"):
                    raise ValueError("node final report disagrees with controller")
            node_results.append({"node": node, "status": "complete", "report_sha256": sha256(node_directory / "report.json")})
        except (OSError, ValueError, KeyError, TypeError) as exc:
            errors.append(f"node {node} verification: {type(exc).__name__}: {exc}")
            node_results.append({"node": node, "status": "failed"})
    status = "complete" if control_report["status"] == "complete" and not errors else "partial"
    result = {"schema_version": 1, "status": status, "errors": errors, "interrupted": interrupted,
              "cleanup_unconfirmed_nodes": cleanup_unconfirmed,
              "elapsed_seconds": time.monotonic() - began, "finished_at": utc_now(), "nodes": node_results}
    write_json(directory / "result.json", result)
    return {"trial": directory.name, "config": config, **case, **result, "controller": control_report}


def summarize(directory, quiet=False):
    manifest = read_json(directory / "manifest.json")
    if manifest.get("mode") != "d28_fly_campaign":
        raise ValueError("not a D28 Fly campaign")
    trials = []
    for path in sorted(directory.glob("trial-*/trial.json")):
        case = read_json(path)
        result = read_json(path.parent / "result.json")
        controller = read_json(path.parent / "controller.json")
        trials.append({**case, **result, "controller": controller})
    rows = aggregate(trials)
    if not quiet:
        print("nodes offline headers solution profile complete/trials heights body_B/s receipt_all_p50_ms")
        for row in rows:
            percentiles = row["receipt_ms"]["all"]["percentiles"]
            latency = "-" if percentiles is None else f"{percentiles['p50']:.3f}"
            rate = row["body_bytes_per_second"]
            print(row["nodes"], row["offline"], row["headers"], row["solution_bytes"], row["profile"],
                  f"{row['complete_trials']}/{row['trials']}", row["completed_heights"],
                  "-" if rate is None else f"{rate:.3f}", latency)
        for note in NOTES:
            print(note)
    return {"schema_version": 1, "cases": rows, "notes": NOTES}


def campaign(args):
    roster = validate_roster(read_json(args.roster))
    token = read_token(args.token_file)
    binary = args.binary.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("binary must be an executable file")
    if 3 * (len(roster["peers"]) - args.offline) <= 2 * len(roster["peers"]):
        raise ValueError("online nodes must retain >2/3 quorum")
    cases = schedule(args.headers, args.profiles, args.repetitions, args.seed)
    if len(cases) > 128:
        raise ValueError("campaign limited to 128 sequential trials")
    output = args.out.resolve()
    output.mkdir(mode=0o700, parents=True)
    metadata = machine_metadata(binary)
    write_json(output / "manifest.json", {
        "schema_version": 1, "mode": "d28_fly_campaign", "host": metadata, "roster": roster,
        "cases_in_order": cases, "seed": args.seed, "heights": args.heights, "warmup": args.warmup,
        "solution_bytes": args.solution_bytes, "offline": args.offline, "max_body": MAX_BODY,
        "timeout_secs": args.timeout_secs, "lifetime_secs": args.lifetime_secs,
        "profiles": PROFILES, "notes": NOTES,
        "node_latency_scope": "node-local ACK/tip publication to push_decided callback, not exact INIT timing",
        "script_sha256": sha256(Path(__file__))})
    # Pin every active VM's actual executable before any process is started.
    for node in range(1, len(roster["peers"]) - args.offline):
        info = worker_request(roster["workers"][node], token, "info")
        write_json(output / f"worker-{node:03d}-preflight.json", info)
        if info.get("binary_sha256") != metadata["binary_sha256"]:
            raise ValueError(f"node {node} binary hash differs from local node0")
    deadline = time.monotonic() + args.lifetime_secs
    results = []
    for index, case in enumerate(cases, 1):
        if time.monotonic() >= deadline:
            break
        name = f"trial-{index:03d}-r{case['repetition']:02d}-h{case['headers']}-{case['profile']}"
        result = run_trial(args, roster, token, binary, metadata, output / name, case, deadline)
        results.append({"trial": name, "status": result["status"]})
        print(f"{name}: {result['status']}", flush=True)
        if result["interrupted"] or result["cleanup_unconfirmed_nodes"]:
            break
    complete = len(results) == len(cases) and all(row["status"] == "complete" for row in results)
    write_json(output / "run.json", {"status": "complete" if complete else "partial",
                                     "finished_at": utc_now(), "planned_trials": len(cases), "trials": results})
    write_json(output / "summary.json", summarize(output))
    return 0 if complete else 1


def fixture_config(**changes):
    config = {"node": 0, "nodes": 4, "offline": 0, "headers": 32, "solution_bytes": 0,
              "heights": 2, "warmup": 0, "timeout_secs": 5, "workers": 1, "max_body": MAX_BODY,
              "chatter_secs": 0, "peers": [f"[::1]:{20000 + i}" for i in range(4)],
              "coordinator": "[::1]:21000", "run_id": "fixture-0123456789", "max_requests": 2,
              "max_outgoing": 2, **PROFILES["baseline"]}
    config.update(changes)
    return config


class SelfTests(unittest.TestCase):
    def ready(self, state):
        for node in range(state.online):
            state.ready({"type": "ready", "node": node, "run_id": state.config["run_id"]}, 1)
        state.start(2)

    def decision(self, state, node=0, **changes):
        message = {"type": "decision", "run_id": state.config["run_id"], "node": node,
                   "height": state.height, "id": "a" * 64, "body_bytes": 100, "payload_bytes": 80,
                   "latency_ms": 1, "round": 0}
        message.update(changes)
        return message

    def test_four_node_campaign_and_failed_zero_decision(self):
        # Synthetic metadata-only peers test orchestration, NOT the D28 protocol.
        fixture = '''import json,socket,sys,time
from pathlib import Path
config=json.loads(Path(sys.argv[2]).read_text())
node=config['node']; rows=[]; status='partial'
host,port=config['coordinator'][1:].split(']:')
sock=socket.socket(socket.AF_INET6,socket.SOCK_STREAM); sock.settimeout(10)
sock.connect((host,int(port))); stream=sock.makefile('rb')
def send(value): sock.sendall(json.dumps(value).encode()+b'\\n')
def receive(): return json.loads(stream.readline())
try:
    send({'type':'ready','run_id':config['run_id'],'node':node})
    assert receive()=={'type':'start'}
    if config['headers']==0: time.sleep(10)
    for height in range(1,config['heights']+config['warmup']+1):
        identifier=('b' if config['headers']==128 and node==1 else 'a')*64
        decision={'type':'decision','run_id':config['run_id'],'node':node,'height':height,
                  'id':identifier,'body_bytes':100,'payload_bytes':80,'latency_ms':1.25,'round':0}
        send(decision)
        ack=receive(); assert ack=={'type':'ack','height':height}
        rows.append({'height':height,'id':identifier,'body_bytes':100,'payload_bytes':80,
                     'acknowledged':True})
    status='complete'
except (OSError,ValueError,AssertionError): pass
finally:
    sock.close()
    Path(sys.argv[4]).write_text(json.dumps({'schema_version':1,'mode':'distributed',
        'config':config,'status':status,'heights':rows}))
'''
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            root = Path(temporary)
            binary = root / "synthetic-peer"
            binary.write_text("#!" + sys.executable + "\n" + fixture)
            binary.chmod(0o700)
            token_file = root / "token"
            create_token(token_file)
            token = read_token(token_file)
            workers, servers, threads = [], [], []
            try:
                for index in range(1, 4):
                    worker = Worker(binary, root / f"worker-{index}", "lo")
                    server = WorkerServer(("::1", 0), WorkerHandler)
                    server.worker, server.token = worker, token
                    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True)
                    thread.start()
                    workers.append(worker); servers.append(server); threads.append(thread)
                with socket.socket(socket.AF_INET6, socket.SOCK_STREAM) as reserved:
                    reserved.bind(("::1", 0))
                    control = f"[::1]:{reserved.getsockname()[1]}"
                roster = {"peers": fixture_config()["peers"], "coordinator": control, "interface": "lo",
                          "workers": [None] + [f"[::1]:{server.server_address[1]}" for server in servers]}
                write_json(root / "roster.json", roster)
                # Success, zero-decision timeout and a disagreement all retained.
                for name, sizes, expected in (("success", "32", 0), ("failures", "0,128", 1)):
                    args = build_parser().parse_args(["campaign", "--binary", str(binary),
                        "--token-file", str(token_file), "--roster", str(root / "roster.json"),
                        "--out", str(root / name), "--headers", sizes, "--repetitions", "1",
                        "--heights", "3", "--warmup", "1" if name == "success" else "0",
                        "--timeout-secs", "1", "--ready-timeout-secs", "5"])
                    with contextlib.redirect_stdout(io.StringIO()):
                        self.assertEqual(campaign(args), expected)
                    manifest = read_json(root / name / "manifest.json")
                    self.assertEqual(manifest["host"]["binary_sha256"], sha256(binary))
                    summary = read_json(root / name / "summary.json")
                    if name == "success":
                        row = summary["cases"][0]
                        self.assertEqual(row["completed_heights"], 3)
                        self.assertEqual(row["body_bytes"], 300)
                        self.assertEqual(row["node_local_ms"]["n"], 12)
                    else:
                        self.assertEqual(sum(row["completed_heights"] for row in summary["cases"]), 0)
                        zero = next(row for row in summary["cases"] if row["headers"] == 0)
                        self.assertGreaterEqual(zero["duration_seconds"], 1)
                        self.assertEqual(zero["body_bytes_per_second"], 0)
                        failures = [read_json(path)["failure"] for path in (root / name).glob("trial-*/controller.json")]
                        self.assertTrue(any("conflicting" in value for value in failures), failures)
                    for path in (root / name).glob("trial-*/node-*/worker.json"):
                        self.assertIsInstance(read_json(path)["process_window"]["start"], dict)
            finally:
                for server in servers:
                    server.shutdown(); server.server_close()
                for thread in threads:
                    thread.join(2)
                for worker in workers:
                    worker.stop_all()

    def test_seeded_matched_schedule(self):
        first = schedule([32, 128, 512, 2048], ["baseline", "fast"], 3, 428)
        self.assertEqual(first, schedule([32, 128, 512, 2048], ["baseline", "fast"], 3, 428))
        self.assertNotEqual(first, schedule([32, 128, 512, 2048], ["baseline", "fast"], 3, 429))
        for repetition in range(1, 4):
            rows = [row for row in first if row["repetition"] == repetition]
            self.assertEqual(len(rows), 8)
            for index in range(0, len(rows), 2):
                self.assertEqual(rows[index]["headers"], rows[index + 1]["headers"])
                self.assertEqual({rows[index]["profile"], rows[index + 1]["profile"]}, {"baseline", "fast"})

    def test_agreement_and_warmup(self):
        state = Barrier(fixture_config(warmup=1, heights=1), 0)
        self.ready(state)
        for height in (1, 2):
            for node in range(4):
                self.assertEqual(state.decision(self.decision(state, node), height + 2), node == 3)
            state.acked(height + 2.1, height + 2.2)
        report = state.report(10)
        self.assertEqual(report["status"], "complete")
        self.assertAlmostEqual(report["rate_duration_seconds"], 1.1)
        self.assertEqual([r["measured"] for r in report["heights"]], [False, True])

    def test_cycles_do_not_double_count_ack_send(self):
        state = Barrier(fixture_config(warmup=1, heights=2), 0)
        self.ready(state)
        for began in (3, 6, 9):
            for node in range(4):
                state.decision(self.decision(state, node), began - 0.5)
            state.acked(began, began + 1)
        report = state.report(10)
        measured = [row for row in report["heights"] if row["measured"]]
        self.assertEqual(report["rate_duration_seconds"], 7)
        self.assertEqual(sum(row["cycle_ms"] for row in measured), 6000)
        self.assertEqual(sum(row["cycle_ms"] for row in measured) + measured[-1]["ack_send_ms"],
                         report["rate_duration_seconds"] * 1000)

    def test_conflict_duplicate_unready_and_height(self):
        state = Barrier(fixture_config(), 0)
        with self.assertRaises(ProtocolError):
            state.decision(self.decision(state), 1)
        self.ready(state)
        state.decision(self.decision(state), 3)
        for message in (self.decision(state), self.decision(state, 1, id="b" * 64),
                        self.decision(state, 1, body_bytes=101), self.decision(state, 1, height=2),
                        self.decision(state, 1, run_id="wrong")):
            with self.assertRaises(ProtocolError):
                state.decision(message, 3)
        with self.assertRaises(ProtocolError):
            state.acked(3, 4)

    def test_partial_zero_and_aggregation_rates(self):
        config = fixture_config(heights=1)
        full = Barrier(config, 0)
        self.ready(full)
        for node in range(4):
            full.decision(self.decision(full, node), 3)
        full.acked(3, 4)
        empty = Barrier(config, 0)
        self.ready(empty)
        empty.finish(5, "timeout")
        partial = Barrier(config, 0)
        self.ready(partial)
        partial.decision(self.decision(partial), 3)
        partial.finish(7, "disconnect")
        rows = [{"config": config, "profile": "baseline", "status": s.report(10)["status"],
                 "controller": s.report(10)} for s in (full, empty, partial)]
        result = aggregate(rows)[0]
        self.assertEqual(result["trials"], 3)
        self.assertEqual(result["complete_trials"], 1)
        self.assertEqual(result["duration_seconds"], 10)
        self.assertEqual(result["body_bytes"], 100)  # NOT 4 * 100
        self.assertEqual(result["body_bytes_per_second"], 10)
        self.assertEqual(result["receipt_ms"]["first"]["n"], 2)
        self.assertEqual(result["receipt_ms"]["all"]["n"], 1)
        failed_ready = Barrier(config, 10)
        failed_ready.finish(12, "ready timeout")
        self.assertEqual(failed_ready.report(12)["rate_duration_seconds"], 2)

    def test_post_ack_collection_failure_confirms_exact_run_cleanup(self):
        roster = {"peers": fixture_config()["peers"], "coordinator": "[::1]:21000", "interface": "lo",
                  "workers": [None, "[::1]:22001", "[::1]:22002", "[::1]:22003"]}

        def controller(config, _ready_timeout):
            state = Barrier(config, 0)
            self.ready(state)
            for node in range(4):
                state.decision(self.decision(state, node), 3)
            state.acked(3, 4)
            result = mock.Mock(state=state)
            result.done.wait.return_value = result.done.is_set.return_value = True
            result.report.return_value = state.report(4)
            return result

        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            root = Path(temporary)
            for confirmed in (False, True):
                def request(_address, _token, operation, value):
                    if operation == "start":
                        return {"accepted": True, "node": value["node"], "run_id": value["run_id"]}
                    self.assertIn(operation, ("stop", "status"))
                    self.assertEqual(set(value), {"run_id"})
                    return {"run_id": value["run_id"], "done": confirmed}

                args = build_parser().parse_args(["campaign", "--binary", sys.executable, "--roster", "unused",
                    "--token-file", "unused", "--out", "unused", "--heights", "1", "--warmup", "0"])
                local = mock.Mock()
                local.done.wait.return_value = local.done.is_set.return_value = True
                with mock.patch(__name__ + ".Coordinator", side_effect=controller), \
                        mock.patch(__name__ + ".NodeJob", return_value=local), \
                        mock.patch(__name__ + ".worker_request", side_effect=request) as remote, \
                        mock.patch(__name__ + ".collect_remote", side_effect=TimeoutError("synthetic post-ACK timeout")), \
                        mock.patch(__name__ + ".validate_node_artifacts", side_effect=ValueError("synthetic absent artifact")):
                    result = run_trial(args, roster, "not-sent", Path(sys.executable), {"binary_sha256": "a" * 64},
                                       root / str(confirmed), {"headers": 32, "profile": "baseline", "repetition": 1},
                                       time.monotonic() + 60)
                self.assertEqual(result["controller"]["status"], "complete")
                self.assertEqual(result["status"], "partial")
                stops = [call for call in remote.call_args_list if call.args[2] == "stop"]
                self.assertEqual(len(stops), 3)
                self.assertTrue(all(call.args[3] == {"run_id": result["config"]["run_id"]} for call in stops))
                self.assertEqual(result["cleanup_unconfirmed_nodes"], [] if confirmed else [1, 2, 3])
                self.assertEqual(read_json(root / str(confirmed) / "result.json")["cleanup_unconfirmed_nodes"],
                                 result["cleanup_unconfirmed_nodes"])

    def test_campaign_halts_when_cleanup_unconfirmed(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            root = Path(temporary)
            create_token(root / "token")
            write_json(root / "roster.json", {"peers": ["[::1]:20000"], "workers": [None],
                                              "coordinator": "[::1]:21000"})
            args = build_parser().parse_args(["campaign", "--binary", sys.executable,
                "--roster", str(root / "roster.json"), "--token-file", str(root / "token"),
                "--out", str(root / "out"), "--headers", "32", "--repetitions", "2"])
            with mock.patch(__name__ + ".run_trial", return_value={"status": "partial", "interrupted": False,
                            "cleanup_unconfirmed_nodes": [1]}) as trial, \
                    mock.patch(__name__ + ".summarize", return_value={}), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(campaign(args), 1)
            self.assertEqual(trial.call_count, 1)
            result = read_json(root / "out" / "run.json")
            self.assertEqual(result["planned_trials"], 2)
            self.assertEqual(len(result["trials"]), 1)

    def test_post_popen_start_failure_stops_owned_child(self):
        binary = Path(__file__).resolve()
        original_start = threading.Thread.start
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            for error_type in (KeyboardInterrupt, RuntimeError, OSError):
                for fail_at in range(3):  # First reader, second reader, watcher.
                    with self.subTest(error=error_type.__name__, thread=fail_at):
                        child = mock.Mock(stdout=io.BytesIO(), stderr=io.BytesIO())
                        count = 0

                        def start(thread):
                            nonlocal count
                            index = count
                            count += 1
                            if index == fail_at:
                                raise error_type("synthetic thread start failure")
                            return original_start(thread)

                        directory = Path(temporary) / f"{error_type.__name__}-{fail_at}"
                        with mock.patch("subprocess.Popen", return_value=child) as popen, \
                                mock.patch("threading.Thread.start", new=start), \
                                mock.patch(__name__ + ".machine_metadata", return_value={"binary_sha256": sha256(binary)}), \
                                mock.patch(__name__ + ".stop_child") as stop:
                            if error_type is OSError:
                                job = NodeJob(binary, sha256(binary), directory, fixture_config(), "lo")
                                self.assertTrue(job.done.is_set())
                                self.assertIn("synthetic", job.result["error"])
                            else:
                                with self.assertRaises(error_type):
                                    NodeJob(binary, sha256(binary), directory, fixture_config(), "lo")
                            stop.assert_called_once_with(child)
                            self.assertTrue(popen.call_args.kwargs["start_new_session"])
                        self.assertTrue(child.stdout.closed)
                        self.assertTrue(child.stderr.closed)

    def test_worker_fixed_launch_auth_and_artifacts(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            root = Path(temporary)
            token_file = root / "token"
            create_token(token_file)
            token = read_token(token_file)
            with self.assertRaises(FileExistsError):
                create_token(token_file)
            token_file.chmod(0o644)
            with self.assertRaises(ValueError):
                read_token(token_file)
            token_file.chmod(0o600)
            binary = root / "synthetic-binary"
            binary.write_text("#!" + sys.executable + "\nimport json,sys\nfrom pathlib import Path\n"
                              "Path(sys.argv[4]).write_text(json.dumps({'status':'complete'}))\n"
                              "print('x' * 300000)\n")
            binary.chmod(0o700)
            worker = Worker(binary, root / "worker", "lo", 2)
            with WorkerServer(("::1", 0), WorkerHandler) as server:
                address = f"[::1]:{server.server_address[1]}"
                server.worker, server.token = worker, token
                thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True)
                thread.start()
                try:
                    with self.assertRaises(ValueError):
                        worker_request(address, "0" * 64, "info")
                    self.assertEqual(worker_request(address, token, "info")["binary_sha256"], sha256(binary))
                    with self.assertRaises(ValueError):
                        worker_request(address, token, "start", fixture_config(binary="/bin/sh"))
                    config = fixture_config()
                    worker_request(address, token, "start", config)
                    job = worker.jobs[config["run_id"]]
                    self.assertTrue(job.done.wait(5))
                    self.assertEqual(job.result["returncode"], 0)
                    with self.assertRaises(ValueError):
                        worker_request(address, token, "start", config)
                    artifacts = worker_request(address, token, "result", {"run_id": config["run_id"]})
                    self.assertEqual(decode(artifacts["files"]["report.json"])["status"], "complete")
                    self.assertEqual(len(artifacts["files"]["stdout.txt"]), MAX_LOG)
                    self.assertTrue(job.result["logs"]["stdout"]["truncated"])
                    self.assertEqual(decode(artifacts["files"]["config.json"]), config)
                finally:
                    server.shutdown()
                    thread.join(2)
                    worker.stop_all()

    def test_snapshot_missing_cgroup_and_counter_direction(self):
        values = {"/proc/net/dev": "eth0: 20 2 0 0 0 0 0 0 30 3 0 0 0 0 0 0\n",
                  "/proc/stat": "cpu 1 2 3 4 5 6 7 8 9 10\n"}
        with mock.patch(__name__ + ".optional_text", side_effect=values.get):
            snapshot = system_snapshot("eth0")
        self.assertEqual(snapshot["network"]["tx_bytes"], 30)
        self.assertEqual(snapshot["cpu_jiffies"]["steal"], 8)
        self.assertIsNone(snapshot["cgroup_cpu_stat"])
        self.assertIsNone(deltas(None, {"tx_bytes": 0}))
        self.assertIsNone(deltas({"tx_bytes": 1}, {"tx_bytes": 0}))

    def test_validation(self):
        for value in ("[::]:9000", "[2001:4860::1]:9000", "127.0.0.1:9000", "[::1]:80"):
            with self.assertRaises(ValueError):
                endpoint(value)
        for change in ({"headers": 5500}, {"workers": 2}, {"max_body": MAX_BODY + 1},
                       {"node": True}, {"offline": 2}, {"run_id": "../../escape"},
                       {"binary": "/bin/sh"}, {"poll_ms": 6}):
            with self.assertRaises(ValueError):
                validate_config(fixture_config(**change))
        with self.assertRaises(ValueError):
            decode('{"a":1,"a":2}')
        with self.assertRaises(ValueError):
            decode('{"a":NaN}')


class RealTests(unittest.TestCase):
    binary = None

    def test_four_real_nodes(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            root = Path(temporary)
            token_file = root / "token"
            create_token(token_file)
            token = read_token(token_file)
            servers, threads, workers = [], [], []
            try:
                for node in range(1, 4):
                    worker = Worker(self.binary, root / f"worker-{node}", "lo")
                    server = WorkerServer(("::1", 0), WorkerHandler)
                    server.worker, server.token = worker, token
                    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True)
                    thread.start()
                    servers.append(server); threads.append(thread); workers.append(worker)
                # Hold distinct UDP ports while allocating; Rust owns them after release.
                with contextlib.ExitStack() as stack:
                    ports = []
                    for _ in range(4):
                        reserved = stack.enter_context(socket.socket(socket.AF_INET6, socket.SOCK_DGRAM))
                        reserved.bind(("::1", 0))
                        ports.append(reserved.getsockname()[1])
                    listener = stack.enter_context(socket.socket(socket.AF_INET6, socket.SOCK_STREAM))
                    listener.bind(("::1", 0))
                    control = f"[::1]:{listener.getsockname()[1]}"
                roster = {"peers": [f"[::1]:{port}" for port in ports], "coordinator": control,
                          "interface": "lo", "workers": [None] + [f"[::1]:{s.server_address[1]}" for s in servers]}
                write_json(root / "roster.json", roster)
                args = build_parser().parse_args(["campaign", "--binary", str(self.binary),
                    "--roster", str(root / "roster.json"), "--token-file", str(token_file),
                    "--out", str(root / "smoke"), "--headers", "32", "--repetitions", "1",
                    "--heights", "3", "--warmup", "1", "--timeout-secs", "30"])
                result = campaign(args)
                details = {str(path.relative_to(root)): optional_text(path) for pattern in
                           ("trial-*/result.json", "trial-*/controller.json", "trial-*/node-*/stderr.txt")
                           for path in (root / "smoke").glob(pattern)}
                self.assertEqual(result, 0, details)
                summary = read_json(root / "smoke" / "summary.json")["cases"][0]
                self.assertEqual(summary["completed_heights"], 3)
                self.assertGreater(summary["body_bytes"], 18000)
                self.assertEqual(summary["receipt_ms"]["all"]["n"], 3)
            finally:
                for server in servers:
                    server.shutdown(); server.server_close()
                for thread in threads:
                    thread.join(2)
                for worker in workers:
                    worker.stop_all()


def bounded_arg(low, high):
    def parse(text):
        try:
            return integer(int(text), low, high, "argument")
        except (ValueError, TypeError) as exc:
            raise argparse.ArgumentTypeError(str(exc)) from None
    return parse


def comma_arg(parse):
    def values(text):
        try:
            result = [parse(item) for item in text.split(",")]
        except ValueError as exc:
            raise argparse.ArgumentTypeError(str(exc)) from None
        if not result or len(result) != len(set(result)):
            raise argparse.ArgumentTypeError("need a nonempty list with no duplicates")
        return result
    return values


def profile_arg(text):
    if text not in PROFILES:
        raise ValueError("profile must be baseline or fast")
    return text


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="mode", required=True)
    tests = commands.add_parser("selftest", help="local unit and synthetic TCP tests; no Fly access")
    tests.add_argument("--binary", type=Path, help="also smoke-test four real local Rust processes")
    token = commands.add_parser("token", help="write NEW 0600 random token file; never print its content")
    token.add_argument("--out", required=True, type=Path)
    summary = commands.add_parser("summary", help="read retained controller results; no network")
    summary.add_argument("directory", type=Path)
    worker = commands.add_parser("worker", help="one fixed binary; explicit private IPv6 bind, bounded lifetime")
    worker.add_argument("--bind", required=True, help="[6PN IPv6]:9200; no public or wildcard bind")
    worker.add_argument("--binary", required=True, type=Path)
    worker.add_argument("--token-file", required=True, type=Path)
    worker.add_argument("--root", required=True, type=Path, help="NEW artifact directory")
    worker.add_argument("--interface", default="eth0")
    worker.add_argument("--max-runs", type=bounded_arg(1, 128), default=128)
    worker.add_argument("--lifetime-secs", type=bounded_arg(1, 7200), default=7200)
    command = commands.add_parser("campaign", help="ON VM0: local node0 + private remote workers, sequential randomized grid")
    command.add_argument("--roster", required=True, type=Path)
    command.add_argument("--binary", required=True, type=Path)
    command.add_argument("--token-file", required=True, type=Path)
    command.add_argument("--out", required=True, type=Path, help="NEW artifact directory")
    command.add_argument("--headers", type=comma_arg(bounded_arg(0, 2048)), default=[32, 128, 512, 2048])
    command.add_argument("--solution-bytes", type=bounded_arg(0, 1344), default=0,
                         help="applies to all sizes; near-cap arm: --headers 2048 --solution-bytes 320")
    command.add_argument("--profiles", type=comma_arg(profile_arg), default=["baseline"],
                         help="baseline or baseline,fast; host polling only, never protocol timers")
    command.add_argument("--repetitions", type=bounded_arg(1, 32), default=3)
    command.add_argument("--seed", type=bounded_arg(0, 2**32 - 1), default=428)
    command.add_argument("--heights", type=bounded_arg(1, 200), default=12)
    command.add_argument("--warmup", type=bounded_arg(0, 20), default=2)
    command.add_argument("--offline", type=bounded_arg(0, 31), default=0)
    command.add_argument("--timeout-secs", type=bounded_arg(1, 1800), default=180)
    command.add_argument("--ready-timeout-secs", type=bounded_arg(1, 120), default=30)
    command.add_argument("--chatter-secs", type=bounded_arg(0, 30), default=0)
    command.add_argument("--lifetime-secs", type=bounded_arg(1, 7000), default=6600)
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        if args.mode == "selftest":
            suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTests)
            if args.binary is not None:
                RealTests.binary = args.binary.resolve(strict=True)
                suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(RealTests))
            return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
        if args.mode == "token":
            create_token(args.out)
            return 0
        if args.mode == "summary":
            summarize(args.directory.resolve(strict=True))
            return 0
        if args.mode == "worker":
            endpoint(args.bind)
            return serve_worker(args)
        return campaign(args)
    except (OSError, ValueError, http.client.HTTPException) as exc:
        print(f"d28_fly: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("d28_fly: interrupted; owned children stopped", file=sys.stderr)
        return 130


def stop_signal(_signum, _frame):
    # Convert service shutdown into the same owned-child cleanup as Ctrl-C.
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, stop_signal)
    sys.exit(main())
