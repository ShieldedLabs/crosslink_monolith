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


MAX_BODY = 1048576  # Legacy default; never silently raised for a byte-target sweep.
MAX_BENCH_BODY = 10 * MAX_BODY
REPORT_GRACE_SECONDS = 8  # Shared post-failure cleanup budget, never measured time.
MAX_JSON = 8 * 1024 * 1024
MAX_LOG = 256 * 1024
MAX_LINE = 4096
PROFILES = {"baseline": {"poll_ms": 25, "status_ms": 100, "receive_budget": 32, "max_requests": 2},
            "fast": {"poll_ms": 5, "status_ms": 25, "receive_budget": 128, "max_requests": 2},
            "fast_k1": {"poll_ms": 5, "status_ms": 25, "receive_budget": 128, "max_requests": 1},
            "economy": {"poll_ms": 100, "status_ms": 100, "receive_budget": 32, "max_requests": 2}}
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


@contextlib.contextmanager
def atomic_text_file(path):
    """Publish a complete new artifact without overwriting an existing path."""
    path = Path(path)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent,
                                         prefix="." + path.name + "-", suffix=".tmp", delete=False) as stream:
            temporary = Path(stream.name)
            yield stream
            stream.flush()
            os.fsync(stream.fileno())
        # link, unlike replace/rename, preserves the original create-only contract.
        os.link(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink()


def write_json(path, value):
    with atomic_text_file(path) as stream:
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
              "body_bytes": (0, MAX_BENCH_BODY), "max_body": (1, MAX_BENCH_BODY), "chatter_secs": (0, 30),
              "poll_ms": (5, 100), "status_ms": (25, 100), "receive_budget": (32, 128),
              "max_requests": (1, 2), "max_outgoing": (2, 2)}
    required = set(bounds) - {"solution_bytes", "body_bytes"}
    if set(config) - (set(bounds) | {"peers", "coordinator", "run_id", "timing_profile"}) or not required <= set(config):
        raise ValueError("unknown or missing config fields")
    for key, (low, high) in bounds.items():
        integer(config.get(key, 0), low, high, key)
    if config.get("timing_profile", "production") not in ("production", "wan"):
        raise ValueError("timing_profile must be production or wan")
    if config.get("body_bytes", 0):
        if config["headers"] != 32 or config.get("solution_bytes", 0) != 0:
            raise ValueError("body_bytes requires headers=32 and solution_bytes=0")
        # Serialized v2 fixed bytes + 32 existing mock headers + full-roster certificate.
        minimum = 71 + 187 * config["headers"] + 96 * config["nodes"]
        if not minimum <= config["body_bytes"] <= config["max_body"]:
            raise ValueError(f"body_bytes must be >= {minimum} and <= max_body; no implicit cap increase")
    if config["node"] >= config["nodes"] - config["offline"]:
        raise ValueError("node must be online")
    if 3 * (config["nodes"] - config["offline"]) <= 2 * config["nodes"]:
        raise ValueError("online nodes must retain >2/3 quorum")
    if {key: config[key] for key in PROFILES["baseline"]} not in PROFILES.values():
        raise ValueError("host-service profile must be one of: " + ", ".join(PROFILES))
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


def configs_match(actual, expected):
    """Only the two new serde defaults may differ; never rewrite raw provenance."""
    if not isinstance(actual, dict) or not isinstance(expected, dict):
        return False
    if any(type(config.get("body_bytes", 0)) is not int for config in (actual, expected)):
        return False
    def normalized(config):
        return dict(config, body_bytes=config.get("body_bytes", 0),
                    timing_profile=config.get("timing_profile", "production"))
    return normalized(actual) == normalized(expected)


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
        integer(message["body_bytes"], 1, self.config["max_body"], "body_bytes")
        if self.config.get("body_bytes", 0) and message["body_bytes"] != self.config["body_bytes"]:
            raise ProtocolError("body byte count differs from exact target")
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


# Read-only postprocessing of retained Rust reports. Never substitute worker.json's
# whole-process counters for an absent measured/chatter boundary.
WINDOW_COUNTERS = {
    "application": ("offered.evidence.bytes", "offered.status.bytes", "offered.hello.bytes",
                    "offered.bare_votes.bytes", "scheduler_budget_discarded.messages",
                    "received.messages", "rejected.messages"),
    "transport": ("outbox_dropped_messages", "outbox_dropped_bytes", "inbox_dropped_messages",
                  "inbox_dropped_bytes", "ring_sent", "ring_received", "ring_receive_dropped_full",
                  "ring_inbound_refused_full", "ring_send_head_blocked", "ring_send_dropped_unknown",
                  "ring_send_dropped_oversized", "send_to_unknown_peer_dropped", "oversized_dropped"),
    "system": ("network.eth0.tx_bytes", "process.user_ticks", "process.system_ticks",
               "cpu.steal_ticks", "cpu.total_ticks"),
}
AGGREGATION_NOTES = [
    "Postprocessing retained schema-1 Rust reports; this aggregator need not be the deployed runner version.",
    "Each counter total/rate/ratio uses only trials with that counter valid on ALL online nodes; coverage and matched denominators are per metric, not interchangeable.",
    "Measured rates sum node deltas / sum matched controller durations; body ratios use unique ACKed controller body bytes once. Missing counters are null, not zero.",
    "Node snapshot boundaries are serial and not atomic across machines; partial trials may end at different failure boundaries. Matched partial trials are explicitly counted.",
    "VM eth0 TX includes TCP/control and other VM activity, not pure BFT bytes or retransmissions. RX is never added; offered application bytes and transport ring messages are not wire bytes/packets.",
    "Chatter is a separate final-height hold, not quiescent idle traffic: rates sum each node's bytes / its actual pooled local window duration, not requested hold seconds or controller duration.",
    "Process CPU seconds use each window's reported ticks_per_second. Steal is sum steal / sum total guest CPU ticks, not a diagnosis of shared-host contention.",
    "Peak RSS is the maximum observed end-window process.max_rss_bytes gauge, a process-lifetime high-water mark (possibly reached in warmup), never a counter delta.",
]


def _window_duration(window, system=False):
    start, end = window["start"], window["end"]
    if system:
        start, end = start["system"], end["system"]
    times = [number(snapshot["at_ms"], 0, float("inf"), "snapshot at_ms") for snapshot in (start, end)]
    duration = (times[1] - times[0]) / 1000
    if duration <= 0:
        raise ValueError("nonpositive window duration")
    return duration


def _window_counter(window, area, key):
    boundaries = [window[side][area] if area else window[side] for side in ("start", "end")]
    if area == "system":
        boundaries = [snapshot["counters"] for snapshot in boundaries]
    start, end = [integer(snapshot[key], 0, 2**64 - 1, key) for snapshot in boundaries]
    delta = integer(window[area + "_delta"][key] if area else window[key], 0, 2**64 - 1, key)
    # Rust application/transport deltas saturate on reset and accept missing start
    # keys. Require both original counters here rather than interpreting that as 0.
    if end < start or delta != end - start:
        raise ValueError("counter reset or inconsistent delta: " + key)
    return delta


def _process_cpu_seconds(window, fields):
    ticks = [integer(window[side]["system"]["counters"]["process.ticks_per_second"],
                     1, 2**64 - 1, "process.ticks_per_second") for side in ("start", "end")]
    if ticks[0] != ticks[1]:
        raise ValueError("process.ticks_per_second changed")
    return sum(_window_counter(window, "system", "process." + key + "_ticks") for key in fields) / ticks[0]


def _cpu_ticks(window, key):
    steal = _window_counter(window, "system", "cpu.steal_ticks")
    total = _window_counter(window, "system", "cpu.total_ticks")
    if not 0 <= steal <= total or total == 0:
        raise ValueError("invalid CPU steal/total interval")
    return steal if key == "steal" else total


def _aggregate_window_metric(samples, getter, *, system=False, chatter=False, body_ratio=False, gauge=False):
    values, missing = {}, []
    by_trial = {}
    for index, sample in enumerate(samples):
        by_trial.setdefault(sample["index"], []).append(index)
        try:
            if sample["error"]:
                raise ValueError(sample["error"])
            duration = None if gauge else _window_duration(sample["window"], system)
            values[index] = (getter(sample["window"]), duration)
        except (KeyError, TypeError, ValueError) as exc:
            missing.append({"trial": sample["trial"], "node": sample["node"], "reason": str(exc)})
    out = {"expected_node_windows": len(samples), "available_node_windows": len(values), "missing": missing}
    if gauge:
        return {**out, "max": max((value for value, _ in values.values()), default=None)}
    matched = [indices for indices in by_trial.values() if all(index in values for index in indices)]
    indices = [index for cohort in matched for index in cohort]
    trials = [samples[cohort[0]]["source"] for cohort in matched]
    total = sum(values[index][0] for index in indices) if indices else None
    out.update(total=total, matched_trials=len(trials),
               matched_complete_trials=sum(trial["status"] == "complete" for trial in trials),
               matched_partial_trials=sum(trial["status"] != "complete" for trial in trials),
               excluded_trials=[samples[cohort[0]]["trial"] for cohort in by_trial.values()
                                if not all(index in values for index in cohort)])
    if chatter:
        node_rates = {}
        for index in indices:
            node = samples[index]["node"]
            row = node_rates.setdefault(node, {"node": node, "n": 0, "total": 0, "duration_seconds": 0})
            row["n"] += 1
            row["total"] += values[index][0]
            row["duration_seconds"] += values[index][1]
        for row in node_rates.values():
            row["per_second"] = row["total"] / row["duration_seconds"]
        out["node_rates"] = [node_rates[node] for node in sorted(node_rates)]
        out["per_second"] = sum(row["per_second"] for row in node_rates.values()) if node_rates else None
    else:
        duration = sum(trial["controller"]["rate_duration_seconds"] for trial in trials)
        body = sum(row["body_bytes"] for trial in trials for row in trial["controller"]["heights"]
                   if row["measured"] and row["acknowledged"])
        out.update(controller_duration_seconds=duration if trials else None, body_bytes=body if trials else None,
                   per_second=total / duration if total is not None and duration > 0 else None)
        if body_ratio:
            out["per_body_byte"] = total / body if total is not None and body > 0 else None
    return out


def aggregate_node_windows(trials, window_name="measured_window"):
    """Return JSON-ready per-metric matched cohorts; no files, clocks or network.

    trials have the same config/status/controller/profile fields as aggregate(),
    plus node_reports={node_index: original Rust report}. Missing reports/counters
    exclude that trial from that metric's numerator AND denominator, not from the
    independent chain throughput. Chatter cohorts include only requested holds.
    """
    if window_name not in ("measured_window", "chatter_window"):
        raise ValueError("unknown Rust counter window")
    chatter = window_name == "chatter_window"
    samples, observations = [], []
    requested = [trial for trial in trials if not chatter or trial["config"].get("chatter_secs", 0) > 0]
    for index, trial in enumerate(requested):
        for node in range(trial["config"]["nodes"] - trial["config"]["offline"]):
            sample = {"index": index, "trial": trial.get("trial", f"trial-{index + 1:03d}"),
                      "node": node, "source": trial, "window": None, "error": None}
            observation = {"trial": sample["trial"], "node": node}
            try:
                report = trial.get("node_reports", {}).get(node)
                if not isinstance(report, dict):
                    raise ValueError(trial.get("node_report_errors", {}).get(node, "missing node report"))
                if (report.get("schema_version") != 1 or report.get("mode") != "distributed"
                        or report.get("status") not in ("complete", "partial")
                        or not configs_match(report.get("config"), dict(trial["config"], node=node))):
                    raise ValueError("node report schema/identity/config mismatch")
                window = report[window_name]
                observation.update(report_status=report["status"], duration_seconds=_window_duration(window))
                sample["window"] = window
                try:
                    observation["system_duration_seconds"] = _window_duration(window, system=True)
                except (KeyError, TypeError, ValueError):
                    observation["system_duration_seconds"] = None
                observation["system_errors"] = {
                    side: (window[side]["system"].get("errors", []) if isinstance(window[side].get("system"), dict)
                           else ["missing system snapshot"]) for side in ("start", "end")}
            except (KeyError, TypeError, ValueError) as exc:
                sample["error"] = str(exc)
                observation["error"] = str(exc)
            samples.append(sample)
            observations.append(observation)
    out = {"requested_trials": len(requested), "expected_node_windows": len(samples),
           "valid_node_windows": sum(sample["error"] is None for sample in samples), "node_windows": observations}
    for area, required in WINDOW_COUNTERS.items():
        keys = set(required)
        if area != "system":
            for sample in samples:
                if sample["window"] and isinstance(sample["window"].get(area + "_delta"), dict):
                    keys.update(sample["window"][area + "_delta"])
        out[area + "_delta"] = {
            key: _aggregate_window_metric(samples, lambda window: _window_counter(window, area, key),
                                          system=area == "system", chatter=chatter,
                                          body_ratio=key.endswith(".bytes") or key == "network.eth0.tx_bytes")
            for key in sorted(keys)}
    for key in ("store_replacements", "store_replacement_bytes"):
        out[key] = _aggregate_window_metric(samples, lambda window: _window_counter(window, None, key),
                                            chatter=chatter, body_ratio=key.endswith("_bytes"))
    out["process_cpu_seconds"] = {
        name: _aggregate_window_metric(samples, lambda window: _process_cpu_seconds(window, fields),
                                       system=True, chatter=chatter)
        for name, fields in (("user", ("user",)), ("system", ("system",)), ("total", ("user", "system")))}
    steal = _aggregate_window_metric(samples, lambda window: _cpu_ticks(window, "steal"), system=True, chatter=chatter)
    total = _aggregate_window_metric(samples, lambda window: _cpu_ticks(window, "total"), system=True, chatter=chatter)
    out["cpu_steal"] = {**steal, "total_ticks": total["total"],
                        "percent": 100 * steal["total"] / total["total"] if total["total"] else None}
    out["cpu_steal"]["steal_ticks"] = out["cpu_steal"].pop("total")
    out["peak_rss_bytes"] = _aggregate_window_metric(samples,
        lambda window: integer(window["end"]["system"]["counters"]["process.max_rss_bytes"],
                               0, 2**64 - 1, "process.max_rss_bytes"), system=True, gauge=True)
    return out


def aggregate(trials):
    """Pool controller observations and optional per-node reports without mutation."""
    groups = {}
    for trial in trials:
        config = trial["config"]
        key = (config["nodes"], config["offline"], config["headers"], config.get("solution_bytes", 0), trial["profile"],
               config.get("body_bytes", 0), config["max_body"], config.get("timing_profile", "production"))
        group = groups.setdefault(key, {"nodes": key[0], "offline": key[1], "headers": key[2],
                                       "solution_bytes": key[3], "profile": key[4], "trials": 0,
                                       "target_body_bytes": key[5], "max_body": key[6], "timing_profile": key[7],
                                       "complete_trials": 0, "duration_seconds": 0, "completed_heights": 0,
                                       "body_bytes": 0, "payload_bytes": 0, "body_sizes": set(),
                                       "receipt_ms": {c: [] for c in ("first", "quorum", "all")},
                                       "node_local_ms": [], "cycle_ms": [], "ack_send_ms": [], "_trials": []})
        group["_trials"].append(trial)
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
        grouped_trials = group.pop("_trials")
        group["failed_trials"] = group["trials"] - group["complete_trials"]
        group["body_sizes"] = sorted(group["body_sizes"])
        group["trial_chronology"] = [
            {"trial": trial.get("trial"), "trial_index": trial.get("trial_index"),
             "repetition": trial.get("repetition"), "status": trial["status"],
             "started_at": trial.get("started_at"), "finished_at": trial.get("finished_at"),
             "elapsed_seconds": trial.get("elapsed_seconds"),
             "rate_duration_seconds": trial["controller"]["rate_duration_seconds"],
             "planned_measured_heights": trial["config"]["heights"],
             "observed_measured_heights": sum(row["measured"] for row in trial["controller"]["heights"]),
             "completed_measured_heights": sum(row["measured"] and row["acknowledged"]
                                               for row in trial["controller"]["heights"])}
            for trial in grouped_trials]
        chronology = group["trial_chronology"]
        for key, select in (("started_at", min), ("finished_at", max)):
            group[key] = select((trial[key] for trial in chronology if trial[key] is not None), default=None)
        elapsed = [trial["elapsed_seconds"] for trial in chronology if trial["elapsed_seconds"] is not None]
        group["elapsed_seconds"] = sum(elapsed) if elapsed else None
        group["elapsed_trial_count"] = len(elapsed)
        for key in ("planned_measured_heights", "observed_measured_heights"):
            group[key] = sum(trial[key] for trial in chronology)
        group["uncompleted_measured_heights"] = group["planned_measured_heights"] - group["completed_heights"]
        for field in ("completed_heights", "body_bytes", "payload_bytes"):
            group[field + "_per_second"] = (group[field] / group["duration_seconds"]
                                              if group["duration_seconds"] > 0 else None)
        for field in ("node_local_ms", "cycle_ms", "ack_send_ms"):
            values = group[field]
            group[field] = {"n": len(values), "percentiles": percentile(values)}
        group["receipt_ms"] = {cohort: {"n": len(values), "percentiles": percentile(values)}
                                for cohort, values in group["receipt_ms"].items()}
        for name in ("measured_window", "chatter_window"):
            group[name] = aggregate_node_windows(grouped_trials, name)
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


def schedule(headers, profiles, repetitions, seed, *, body_bytes=None, max_body=MAX_BODY,
             timing_profile="production"):
    targets = headers if body_bytes is None else body_bytes
    if len(targets) * len(profiles) * repetitions > 128:
        raise ValueError("campaign limited to 128 sequential trials")
    rng = random.Random(seed)
    trials = []
    for repetition in range(1, repetitions + 1):
        sizes = list(targets)
        rng.shuffle(sizes)
        for size in sizes:
            paired = list(profiles)
            rng.shuffle(paired)
            for profile in paired:
                trials.append({"repetition": repetition, "headers": size if body_bytes is None else 32,
                               "body_bytes": 0 if body_bytes is None else size, "max_body": max_body,
                               "timing_profile": timing_profile, "profile": profile})
    return trials


def trial_name(index, case):
    # Preserve legacy directory names at the old defaults; new dimensions are explicit.
    target = case.get("body_bytes", 0)
    size = f"b{target}" if target else f"h{case['headers']}"
    cap = case.get("max_body", MAX_BODY)
    if target or cap != MAX_BODY:
        size += f"-cap{cap}"
    timing = case.get("timing_profile", "production")
    suffix = "" if timing == "production" else "-" + timing
    return f"trial-{index:03d}-r{case['repetition']:02d}-{size}-{case['profile']}{suffix}"


def campaign_cases(args):
    if args.body_bytes is not None and (args.headers_explicit or args.solution_bytes_explicit):
        raise ValueError("--body-bytes excludes explicit --headers and --solution-bytes")
    if args.body_bytes is not None and any(value > args.max_body for value in args.body_bytes):
        raise ValueError("--body-bytes targets must fit --max-body; no implicit cap increase")
    return schedule(args.headers, args.profiles, args.repetitions, args.seed,
                    body_bytes=args.body_bytes, max_body=args.max_body, timing_profile=args.timing_profile)


def trial_config(args, roster, case, identifier):
    config = {"node": 0, "nodes": len(roster["peers"]), "offline": args.offline,
              "headers": case["headers"], "solution_bytes": 0 if case.get("body_bytes", 0) else args.solution_bytes,
              "heights": args.heights, "warmup": args.warmup, "timeout_secs": args.timeout_secs,
              "workers": 1, "max_body": case.get("max_body", args.max_body), "chatter_secs": args.chatter_secs,
              "peers": roster["peers"], "coordinator": roster["coordinator"],
              "run_id": identifier, "max_requests": 2, "max_outgoing": 2, **PROFILES[case["profile"]]}
    # Old executables/config artifacts remain usable when neither new mode is requested.
    if case.get("body_bytes", 0):
        config["body_bytes"] = case["body_bytes"]
    if case.get("timing_profile", args.timing_profile) != "production":
        config["timing_profile"] = case.get("timing_profile", args.timing_profile)
    return validate_config(config)


def failure_report_grace(began, timeout, now, started):
    """Only a started trial near its process deadline gets a bounded report flush."""
    near_deadline = abs(now - (began + timeout)) <= min(REPORT_GRACE_SECONDS, timeout / 10)
    return REPORT_GRACE_SECONDS if started and near_deadline else 0


def collect_remote(address, token, identifier, directory, grace):
    deadline = time.monotonic() + grace
    request = {"run_id": identifier}
    while True:
        status = worker_request(address, token, "status", request, timeout=5)
        if not isinstance(status, dict) or status.get("run_id") != identifier:
            raise ValueError("invalid worker status identity")
        if status.get("done") is True:
            break
        if time.monotonic() >= deadline:
            status = worker_request(address, token, "stop", request, timeout=10)
            if not isinstance(status, dict) or status.get("run_id") != identifier or status.get("done") is not True:
                raise ValueError("worker stop did not confirm terminal child")
            break
        time.sleep(min(0.2, max(0, deadline - time.monotonic())))  # Post-ACK/failure only.
    artifacts = worker_request(address, token, "result", request, timeout=10)
    if artifacts.get("run_id") != identifier or not isinstance(artifacts.get("files"), dict):
        raise ValueError("invalid worker result identity")
    names = {"config.json", "manifest.json", "argv.json", "worker.json", "report.json", "stdout.txt", "stderr.txt"}
    if set(artifacts["files"]) - names:
        raise ValueError("unexpected worker artifact")
    for name, raw in artifacts["files"].items():
        if not isinstance(raw, str) or len(raw.encode("utf-8")) > MAX_JSON:
            raise ValueError("invalid worker artifact text")
        with atomic_text_file(directory / name) as stream:
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
    if not configs_match(report.get("config"), config):
        raise ValueError("node report identity/config mismatch")
    return worker, report


def run_trial(args, roster, token, binary, metadata, directory, case, campaign_deadline):
    config = trial_config(args, roster, case, secrets.token_hex(24))
    directory.mkdir(mode=0o700)
    began, started_at = time.monotonic(), utc_now()
    write_json(directory / "trial.json", {"config": config, "started_at": started_at, **case})
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
        # Freeze the controller/rate boundary BEFORE waiting for any report flush.
        cleanup_began = time.monotonic()
        if controller is not None and controller.done.is_set():
            control_report = controller.report()
        else:
            failed = Barrier(config, began)
            failed.finish(cleanup_began, errors[-1] if errors else "no controller report")
            control_report = failed.report(cleanup_began)
        good_controller = control_report["status"] == "complete" and not errors
        grace = args.chatter_secs + REPORT_GRACE_SECONDS if good_controller else 0
        if not good_controller and not errors and not interrupted and controller is not None and local is not None:
            grace = failure_report_grace(local.began, config["timeout_secs"], cleanup_began,
                                         controller.state.started is not None)
        # One shared report-wait deadline, not N*grace. Bounded HTTP/kill operations
        # can take additional time; terminal-state confirmation is still required.
        cleanup_deadline = cleanup_began + grace
        cleanup_unconfirmed = []
        if local is not None and not local.done.wait(max(0, cleanup_deadline - time.monotonic())):
            local.cancel()
            errors.append("node 0 exit timeout")
        if local is not None and not local.done.is_set():
            cleanup_unconfirmed.append(0)
        for node in attempted:
            try:
                collect_remote(roster["workers"][node], token, config["run_id"],
                               directory / f"node-{node:03d}", max(0, cleanup_deadline - time.monotonic()))
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
              "cleanup_unconfirmed_nodes": cleanup_unconfirmed, "report_grace_seconds": grace,
              "started_at": started_at, "elapsed_seconds": time.monotonic() - began,
              "finished_at": utc_now(), "nodes": node_results,
              "planned_measured_heights": config["heights"],
              "observed_measured_heights": sum(row["measured"] for row in control_report["heights"]),
              "completed_measured_heights": sum(row["measured"] and row["acknowledged"]
                                                for row in control_report["heights"])}
    write_json(directory / "result.json", result)
    return {"trial": directory.name, "config": config, **case, **result, "controller": control_report}


def summarize(directory, quiet=False):
    """Read original artifacts; return JSON-ready analysis without writing summary.json."""
    directory = Path(directory)
    manifest = read_json(directory / "manifest.json")
    if manifest.get("mode") != "d28_fly_campaign":
        raise ValueError("not a D28 Fly campaign")
    trials = []
    for path in sorted(directory.glob("trial-*/trial.json")):
        case = read_json(path)
        result = read_json(path.parent / "result.json")
        controller = read_json(path.parent / "controller.json")
        reports, errors = {}, {}
        for node in range(case["config"]["nodes"] - case["config"]["offline"]):
            try:
                reports[node] = read_json(path.parent / f"node-{node:03d}" / "report.json")
            except (OSError, ValueError) as exc:
                errors[node] = f"{type(exc).__name__}: {exc}"
        trials.append({**case, **result, "trial": path.parent.name, "controller": controller,
                       "node_reports": reports, "node_report_errors": errors})
    rows = aggregate(trials)
    if not quiet:
        def fmt(value, factor=1):
            return "-" if value is None else f"{value * factor:.3f}"

        print("nodes/offline headers+solution target_body_B max_body_B timing profile complete/failed local_n local_p50/p95_ms body_KiB/s VM_TX_Mbit/s TX/body evidence/body budget_discards steal_% peak_RSS_MiB")
        for row in rows:
            measured = row["measured_window"]
            tx = measured["system_delta"]["network.eth0.tx_bytes"]
            evidence = measured["application_delta"]["offered.evidence.bytes"]
            discards = measured["application_delta"]["scheduler_budget_discarded.messages"]
            local = row["node_local_ms"]
            latency = local["percentiles"] or {}
            print(f"{row['nodes']}/{row['offline']}", f"{row['headers']}+{row['solution_bytes']}",
                  row["target_body_bytes"], row["max_body"], row["timing_profile"], row["profile"],
                  f"{row['complete_trials']}/{row['failed_trials']}", local["n"],
                  f"{fmt(latency.get('p50'))}/{fmt(latency.get('p95'))}",
                  fmt(row["body_bytes_per_second"], 1 / 1024), fmt(tx["per_second"], 8 / 1e6),
                  fmt(tx["per_body_byte"]), fmt(evidence["per_body_byte"]), fmt(discards["total"]),
                  fmt(measured["cpu_steal"]["percent"]), fmt(measured["peak_rss_bytes"]["max"], 1 / 2**20))
            print(f"  chronology: first_start={row['started_at'] or '-'} last_finish={row['finished_at'] or '-'} "
                  f"elapsed_s={fmt(row['elapsed_seconds'])}(n={row['elapsed_trial_count']}) "
                  f"rate_duration_s={fmt(row['duration_seconds'])} "
                  f"measured_heights completed/observed/planned={row['completed_heights']}/"
                  f"{row['observed_measured_heights']}/{row['planned_measured_heights']}")
            receipts = []
            for name, cohort in row["receipt_ms"].items():
                latency = cohort["percentiles"] or {}
                receipts.append(f"{name}={fmt(latency.get('p50'))}/{fmt(latency.get('p95'))}(n={cohort['n']})")
            print("  controller_receipt_p50/p95_ms:", " ".join(receipts))
            print(f"  matched/all trials: TX={tx['matched_trials']}/{row['trials']} "
                  f"evidence={evidence['matched_trials']}/{row['trials']} budget={discards['matched_trials']}/{row['trials']} "
                  f"steal={measured['cpu_steal']['matched_trials']}/{row['trials']}; "
                  f"RSS nodes={measured['peak_rss_bytes']['available_node_windows']}/{measured['expected_node_windows']}; "
                  f"TX matched-partial={tx['matched_partial_trials']}")
            chatter = row["chatter_window"]
            if chatter["requested_trials"]:
                tx = chatter["system_delta"]["network.eth0.tx_bytes"]
                evidence = chatter["application_delta"]["offered.evidence.bytes"]
                print(f"  chatter: VM_TX_Mbit/s={fmt(tx['per_second'], 8 / 1e6)} "
                      f"evidence_KiB/s={fmt(evidence['per_second'], 1 / 1024)} "
                      f"matched/requested TX={tx['matched_trials']}/{chatter['requested_trials']} "
                      f"evidence={evidence['matched_trials']}/{chatter['requested_trials']} (actual per-node durations)")
        for note in NOTES + AGGREGATION_NOTES:
            print(note)
    return {"schema_version": 1, "aggregation_version": 2, "cases": rows, "notes": NOTES + AGGREGATION_NOTES}


def campaign(args):
    cases = [dict(case, trial_index=index) for index, case in enumerate(campaign_cases(args), 1)]
    roster = validate_roster(read_json(args.roster))
    # Check every target/full-roster minimum before output creation or any child launch.
    for case in cases:
        trial_config(args, roster, case, "preflight-0123456789")
    token = read_token(args.token_file)
    binary = args.binary.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("binary must be an executable file")
    output = args.out.resolve()
    output.mkdir(mode=0o700, parents=True)
    began, started_at = time.monotonic(), utc_now()
    metadata = machine_metadata(binary)
    write_json(output / "manifest.json", {
        "schema_version": 1, "mode": "d28_fly_campaign", "host": metadata, "roster": roster,
        "started_at": started_at, "cases_in_order": cases, "seed": args.seed,
        "heights": args.heights, "warmup": args.warmup, "body_bytes": args.body_bytes,
        "solution_bytes": args.solution_bytes, "offline": args.offline, "max_body": args.max_body,
        "timing_profile": args.timing_profile,
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
        name = trial_name(index, case)
        result = run_trial(args, roster, token, binary, metadata, output / name, case, deadline)
        results.append({"trial": name, "status": result["status"], **case,
                        **{key: result.get(key) for key in ("started_at", "finished_at", "elapsed_seconds")}})
        print(f"{name}: {result['status']}", flush=True)
        if result["interrupted"] or result["cleanup_unconfirmed_nodes"]:
            break
    complete = len(results) == len(cases) and all(row["status"] == "complete" for row in results)
    write_json(output / "run.json", {"status": "complete" if complete else "partial", "started_at": started_at,
                                     "finished_at": utc_now(), "elapsed_seconds": time.monotonic() - began,
                                     "planned_trials": len(cases), "started_trials": len(results),
                                     "complete_trials": sum(row["status"] == "complete" for row in results),
                                     "trials": results})
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
node=config['node']; rows=[]; status='partial'; body=config.get('body_bytes',0) or 100
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
                  'id':identifier,'body_bytes':body,'payload_bytes':80,'latency_ms':1.25,'round':0}
        send(decision)
        ack=receive(); assert ack=={'type':'ack','height':height}
        rows.append({'height':height,'id':identifier,'body_bytes':body,'payload_bytes':80,
                     'acknowledged':True})
    status='complete'
except (OSError,ValueError,AssertionError): pass
finally:
    sock.close()
    config.setdefault('body_bytes',0); config.setdefault('timing_profile','production')
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
                # Legacy success, exact-byte sweep, zero-decision timeout and disagreement.
                for name, options, expected in (("success", ["--headers", "32"], 0),
                        ("bytes", ["--body-bytes", "131072,10485760", "--max-body", "10485760", "--timing-profile", "wan"], 0),
                        ("failures", ["--headers", "0,128"], 1)):
                    args = build_parser().parse_args(["campaign", "--binary", str(binary),
                        "--token-file", str(token_file), "--roster", str(root / "roster.json"),
                        "--out", str(root / name), "--repetitions", "1",
                        "--heights", "3", "--warmup", "1" if expected == 0 else "0",
                        "--timeout-secs", "1", "--ready-timeout-secs", "5"] + options)
                    with contextlib.redirect_stdout(io.StringIO()):
                        self.assertEqual(campaign(args), expected)
                    manifest = read_json(root / name / "manifest.json")
                    self.assertEqual(manifest["host"]["binary_sha256"], sha256(binary))
                    summary = read_json(root / name / "summary.json")
                    if expected == 0:
                        for row in summary["cases"]:
                            self.assertEqual(row["completed_heights"], 3)
                            self.assertEqual(row["body_bytes"], 3 * (row["target_body_bytes"] or 100))
                            self.assertEqual(row["node_local_ms"]["n"], 12)
                            self.assertIsNotNone(row["started_at"])
                            self.assertEqual(row["elapsed_trial_count"], 1)
                            self.assertEqual(row["trial_chronology"][0]["planned_measured_heights"], 3)
                    else:
                        self.assertEqual(sum(row["completed_heights"] for row in summary["cases"]), 0)
                        zero = next(row for row in summary["cases"] if row["headers"] == 0)
                        self.assertGreaterEqual(zero["duration_seconds"], 1)
                        self.assertEqual(zero["body_bytes_per_second"], 0)
                        failures = [read_json(path)["failure"] for path in (root / name).glob("trial-*/controller.json")]
                        self.assertTrue(any("conflicting" in value for value in failures), failures)
                    if name == "bytes":
                        self.assertEqual(manifest["body_bytes"], [131072, 10485760])
                        self.assertEqual((manifest["max_body"], manifest["timing_profile"]), (10485760, "wan"))
                        self.assertEqual({row["target_body_bytes"] for row in summary["cases"]}, {131072, 10485760})
                        self.assertTrue(all("-cap10485760-" in path.name for path in (root / name).glob("trial-*")))
                    for path in (root / name).glob("trial-*/node-*/worker.json"):
                        worker_result = read_json(path)
                        self.assertIsInstance(worker_result["process_window"]["start"], dict)
                        if expected == 0:
                            self.assertFalse(worker_result["cancelled"])
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

    def counter_trial(self, name="trial-001", duration=2, body=100, scale=1, status="complete", hold_scale=1):
        config = fixture_config(heights=1, warmup=1, chatter_secs=1)
        row = {"measured": True, "acknowledged": True, "body_bytes": body, "payload_bytes": body // 2,
               "cycle_ms": 1000, "ack_send_ms": 1,
               "decisions": [{"node": n, "node_latency_ms": n + 1, "receipt_ms": 100 * (n + 1)} for n in range(4)]}
        trial = {"trial": name, "config": config, "profile": "baseline", "status": status,
                 "controller": {"rate_duration_seconds": duration,
                                "heights": [dict(row, measured=False, body_bytes=999999)] + ([row] if body else [])},
                 "node_reports": {}}
        for node in range(4):
            ticks = 100 if node % 2 == 0 else 250

            def window(seconds, tx):
                at = 10000 * (node + 1)  # Unrelated per-node clock origins.
                application = {key: scale * (node + 1) for key in WINDOW_COUNTERS["application"]}
                application["offered.evidence.bytes"] *= 100
                transport = {key: 0 for key in WINDOW_COUNTERS["transport"]}
                system = {"network.eth0.tx_bytes": tx, "network.eth0.rx_bytes": 10**9,
                          "process.user_ticks": ticks * scale, "process.system_ticks": ticks * scale // 2,
                          "cpu.total_ticks": 100 * (node + 1), "cpu.steal_ticks": 10 * (node + 1)}
                result = {"application_delta": application, "transport_delta": transport, "system_delta": system,
                          "store_replacements": scale * (node + 1),
                          "store_replacement_bytes": scale * (node + 1) * 3152048}
                for side, factor in (("start", 0), ("end", 1)):
                    snapshot = {"at_ms": at + factor * seconds * 1000,
                                "application": {k: 100000 + factor * v for k, v in application.items()},
                                "transport": {k: 100000 + factor * v for k, v in transport.items()},
                                "system": {"at_ms": at + 5 + factor * seconds * 1000, "errors": [],
                                           "counters": {k: 100000 + factor * v for k, v in system.items()}}}
                    snapshot["system"]["counters"].update({"process.ticks_per_second": ticks,
                        "process.max_rss_bytes": 1000 + factor * (1000 + node), "process.rss_bytes": 50})
                    for key in ("store_replacements", "store_replacement_bytes"):
                        snapshot[key] = 10**9 + factor * result[key]
                    result[side] = snapshot
                return result

            trial["node_reports"][node] = {"schema_version": 1, "mode": "distributed", "status": status,
                "config": dict(config, node=node),
                "measured_window": window(duration + node, 200 * (node + 1) * scale),
                "chatter_window": window(hold_scale * (node + 1), 1000 * (node + 1) * scale),
                "process_start": {"network.eth0.tx_bytes": 0},
                "process_end": {"network.eth0.tx_bytes": 10**12}}
        return trial

    def test_counter_rates_exclude_warmup_hold_and_process(self):
        row = aggregate([self.counter_trial(), self.counter_trial("trial-002", duration=8, body=300, scale=3)])[0]
        self.assertEqual((row["trials"], row["complete_trials"], row["failed_trials"]), (2, 2, 0))
        self.assertEqual((row["body_bytes"], row["duration_seconds"], row["body_bytes_per_second"]), (400, 10, 40))
        self.assertEqual(row["node_local_ms"], {"n": 8, "percentiles": {"p50": 2, "p95": 4, "p99": 4}})
        self.assertEqual(row["receipt_ms"]["first"]["percentiles"]["p50"], 100)
        measured = row["measured_window"]
        tx = measured["system_delta"]["network.eth0.tx_bytes"]
        self.assertEqual((tx["total"], tx["per_second"], tx["per_body_byte"]), (8000, 800, 20))
        self.assertEqual((tx["body_bytes"], tx["controller_duration_seconds"]), (400, 10))
        self.assertEqual(measured["application_delta"]["offered.evidence.bytes"]["per_body_byte"], 10)
        self.assertEqual(measured["application_delta"]["received.messages"]["total"], 40)
        self.assertEqual(measured["transport_delta"]["ring_receive_dropped_full"]["total"], 0)
        self.assertNotIn("network.eth0.rx_bytes", measured["system_delta"])
        self.assertEqual(measured["store_replacements"]["total"], 40)
        self.assertEqual(measured["store_replacement_bytes"]["total"], 40 * 3152048)
        self.assertEqual(measured["store_replacement_bytes"]["per_second"], 4 * 3152048)

    def test_counter_missing_coverage_and_partial_denominators(self):
        good, missing = self.counter_trial(), self.counter_trial("trial-002", duration=8, body=300, scale=3)
        del missing["node_reports"][1]["measured_window"]["system_delta"]["network.eth0.tx_bytes"]
        row = aggregate([good, missing])[0]
        tx = row["measured_window"]["system_delta"]["network.eth0.tx_bytes"]
        self.assertEqual(row["body_bytes_per_second"], 40)
        self.assertEqual((tx["total"], tx["body_bytes"], tx["controller_duration_seconds"]), (2000, 100, 2))
        self.assertEqual((tx["matched_trials"], tx["available_node_windows"], tx["expected_node_windows"]), (1, 7, 8))
        self.assertEqual(tx["excluded_trials"], ["trial-002"])
        self.assertEqual(tx["missing"][0]["node"], 1)
        self.assertEqual(row["measured_window"]["application_delta"]["offered.evidence.bytes"]["matched_trials"], 2)
        partial = self.counter_trial("trial-003", duration=5, body=0, status="partial")
        row = aggregate([good, partial])[0]
        tx = row["measured_window"]["system_delta"]["network.eth0.tx_bytes"]
        self.assertEqual(row["failed_trials"], 1)
        self.assertEqual(row["body_bytes_per_second"], 100 / 7)
        self.assertEqual((tx["total"], tx["per_second"], tx["matched_partial_trials"]), (4000, 4000 / 7, 1))
        self.assertIsNone(aggregate_node_windows([partial])["system_delta"]["network.eth0.tx_bytes"]["per_body_byte"])
        partial["node_reports"] = {}
        row = aggregate([good, partial])[0]
        tx = row["measured_window"]["system_delta"]["network.eth0.tx_bytes"]
        self.assertEqual(row["body_bytes_per_second"], 100 / 7)
        self.assertEqual((tx["matched_trials"], len(tx["missing"]), tx["controller_duration_seconds"]), (1, 4, 2))
        self.assertIsNone(aggregate_node_windows([partial])["system_delta"]["network.eth0.tx_bytes"]["total"])

    def test_cpu_units_steal_and_peak_gauge(self):
        trial = self.counter_trial()
        measured = aggregate_node_windows([trial])
        self.assertEqual({k: v["total"] for k, v in measured["process_cpu_seconds"].items()},
                         {"user": 4, "system": 2, "total": 6})
        self.assertEqual(measured["process_cpu_seconds"]["total"]["per_second"], 3)
        self.assertEqual((measured["cpu_steal"]["steal_ticks"], measured["cpu_steal"]["total_ticks"],
                          measured["cpu_steal"]["percent"]), (100, 1000, 10))
        self.assertEqual(measured["peak_rss_bytes"]["max"], 2003)  # Not delta=1003 or sum=8006.
        end = trial["node_reports"][0]["measured_window"]["end"]["system"]["counters"]
        del end["process.ticks_per_second"]
        measured = aggregate_node_windows([trial])
        self.assertIsNone(measured["process_cpu_seconds"]["total"]["total"])
        self.assertEqual(measured["process_cpu_seconds"]["total"]["available_node_windows"], 3)
        self.assertEqual(measured["system_delta"]["network.eth0.tx_bytes"]["total"], 2000)
        trial["node_reports"][0]["measured_window"]["start"]["system"] = None
        measured = aggregate_node_windows([trial])
        self.assertIsNone(measured["system_delta"]["network.eth0.tx_bytes"]["total"])
        self.assertEqual(measured["application_delta"]["offered.evidence.bytes"]["total"], 1000)
        self.assertEqual(measured["peak_rss_bytes"]["available_node_windows"], 4)  # End gauge needs no start counter.
        self.assertEqual(measured["node_windows"][0]["system_errors"]["start"], ["missing system snapshot"])

    def test_invalid_window_counters_are_not_zero(self):
        for fault in ("reset", "missing_start", "bad_delta", "bad_clock", "wrong_node", "changed_ticks"):
            with self.subTest(fault=fault):
                trial = self.counter_trial()
                report = trial["node_reports"][0]
                window = report["measured_window"]
                key = "offered.evidence.bytes"
                if fault == "reset":
                    window["start"]["application"][key] = 10**9
                    window["application_delta"][key] = 0
                elif fault == "missing_start":
                    del window["start"]["application"][key]
                elif fault == "bad_delta":
                    window["application_delta"][key] = True
                elif fault == "bad_clock":
                    window["end"]["at_ms"] = window["start"]["at_ms"]
                elif fault == "wrong_node":
                    report["config"]["node"] = 1
                else:
                    window["end"]["system"]["counters"]["process.ticks_per_second"] = 99
                measured = aggregate_node_windows([trial])
                metric = (measured["process_cpu_seconds"]["total"] if fault == "changed_ticks"
                          else measured["application_delta"][key])
                self.assertIsNone(metric["total"])
                self.assertEqual((metric["matched_trials"], metric["available_node_windows"]), (0, 3))
                self.assertEqual(len(metric["missing"]), 1)

    def test_chatter_rates_use_actual_per_node_durations(self):
        trials = [self.counter_trial(), self.counter_trial("trial-002", scale=3, hold_scale=2)]
        chatter = aggregate_node_windows(trials, "chatter_window")
        tx = chatter["system_delta"]["network.eth0.tx_bytes"]
        self.assertEqual(tx["total"], 40000)
        self.assertAlmostEqual(tx["per_second"], 4 * 4000 / 3)
        self.assertEqual([node["duration_seconds"] for node in tx["node_rates"]], [3, 6, 9, 12])
        self.assertNotIn("per_body_byte", tx)
        self.assertNotIn("controller_duration_seconds", tx)
        trials[0]["config"]["chatter_secs"] = 0
        self.assertEqual(aggregate_node_windows(trials, "chatter_window")["requested_trials"], 1)

    def test_summary_is_read_only_and_reports_missing_artifacts(self):
        trial = self.counter_trial()
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            root = Path(temporary)
            write_json(root / "manifest.json", {"mode": "d28_fly_campaign", "script_sha256": "old-deployed-script"})
            write_json(root / "summary.json", {"old": "preserve byte for byte"})
            path = root / trial["trial"]
            path.mkdir()
            write_json(path / "trial.json", {"config": trial["config"], "profile": trial["profile"]})
            write_json(path / "result.json", {"status": trial["status"]})
            write_json(path / "controller.json", trial["controller"])
            for node in range(4):
                directory = path / f"node-{node:03d}"
                directory.mkdir()
                if node < 2:
                    write_json(directory / "report.json", trial["node_reports"][node])
                elif node == 2:
                    (directory / "report.json").write_text("incomplete JSON", encoding="utf-8")
                (directory / "worker.json").write_text("not read: whole-process counters", encoding="utf-8")
            before = {p: p.read_bytes() for p in root.glob("**/*") if p.is_file()}
            with contextlib.redirect_stdout(io.StringIO()) as stdout:
                summary = summarize(root)
            self.assertIn("VM_TX_Mbit/s", stdout.getvalue())
            self.assertIn("TX=0/1", stdout.getvalue())
            self.assertEqual(summary["aggregation_version"], 2)
            tx = summary["cases"][0]["measured_window"]["system_delta"]["network.eth0.tx_bytes"]
            self.assertIsNone(tx["total"])
            self.assertEqual((tx["available_node_windows"], len(tx["missing"])), (2, 2))
            self.assertEqual(before, {p: p.read_bytes() for p in root.glob("**/*") if p.is_file()})
            encode(summary)  # Entire exported analysis is strict, finite JSON.

    def test_fast_k1_exact_profile_and_default(self):
        for profile, knobs in PROFILES.items():
            self.assertEqual(validate_config(fixture_config(**knobs))["max_requests"], knobs["max_requests"])
            self.assertEqual(profile_arg(profile), profile)
        for changes in ({"max_requests": 1}, {"max_requests": 0}, {"max_requests": 3},
                        {"max_outgoing": 1}, {"max_outgoing": 3}, dict(PROFILES["fast_k1"], status_ms=100),
                        dict(PROFILES["fast_k1"], receive_budget=32)):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                validate_config(fixture_config(**changes))
        argv = ["campaign", "--roster", "unused", "--binary", "unused", "--token-file", "unused", "--out", "unused"]
        self.assertEqual(build_parser().parse_args(argv).profiles, ["baseline"])
        self.assertEqual(build_parser().parse_args(argv + ["--profiles", "fast,fast_k1"]).profiles, ["fast", "fast_k1"])

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
        for change in ({"headers": 5500}, {"workers": 2}, {"max_body": MAX_BENCH_BODY + 1},
                       {"node": True}, {"offline": 2}, {"run_id": "../../escape"},
                       {"binary": "/bin/sh"}, {"poll_ms": 6}):
            with self.assertRaises(ValueError):
                validate_config(fixture_config(**change))
        with self.assertRaises(ValueError):
            decode('{"a":1,"a":2}')
        with self.assertRaises(ValueError):
            decode('{"a":NaN}')

    def test_exact_byte_config_and_metadata_bounds(self):
        config = fixture_config(nodes=7, peers=[f"[::1]:{20000 + i}" for i in range(7)],
                                body_bytes=6727, max_body=MAX_BENCH_BODY, timing_profile="wan")
        self.assertEqual(validate_config(config), config)
        self.assertEqual(validate_config(dict(config, offline=2))["body_bytes"], 6727)
        for changes in ({"body_bytes": 6726}, {"body_bytes": True}, {"body_bytes": -1},
                        {"body_bytes": MAX_BENCH_BODY + 1}, {"max_body": 0}, {"max_body": 6726},
                        {"max_body": MAX_BENCH_BODY + 1}, {"headers": 31}, {"solution_bytes": 1},
                        {"timing_profile": "adaptive"}, {"body_bytes_extra": 1}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                validate_config(dict(config, **changes))
        legacy = fixture_config(solution_bytes=320, headers=2048)
        self.assertEqual(validate_config(legacy), legacy)
        self.assertNotIn("body_bytes", legacy)
        self.assertEqual((MAX_JSON, MAX_LINE), (8 * 1024 * 1024, 4096))
        state = Barrier(dict(config, body_bytes=MAX_BENCH_BODY), 0)
        self.ready(state)
        state.decision(self.decision(state, body_bytes=MAX_BENCH_BODY), 3)
        with self.assertRaises(ProtocolError):
            state.decision(self.decision(state, 1, body_bytes=MAX_BENCH_BODY - 1), 3)
        with self.assertRaises(ValueError):
            state.decision(self.decision(state, 1, body_bytes=MAX_BENCH_BODY + 1), 3)
        self.assertLess(len(encode(self.decision(state, body_bytes=MAX_BENCH_BODY))), MAX_LINE)

    def test_body_cli_schedule_and_artifact_names(self):
        argv = ["campaign", "--roster", "unused", "--binary", "unused", "--token-file", "unused", "--out", "unused"]
        parser = build_parser()
        legacy = parser.parse_args(argv)
        self.assertEqual((legacy.headers, legacy.solution_bytes, legacy.max_body, legacy.timing_profile),
                         ([32, 128, 512, 2048], 0, MAX_BODY, "production"))
        self.assertEqual(trial_name(1, {"repetition": 1, "headers": 32, "profile": "baseline"}),
                         "trial-001-r01-h32-baseline")
        targets = [131072, 524288, 1048576, 2097152, 4194304, 8388608, 10485760]
        options = ["--body-bytes", ",".join(map(str, targets)), "--max-body", str(MAX_BENCH_BODY),
                   "--timing-profile", "wan", "--profiles", "baseline,fast"]
        args = parser.parse_args(argv + options)
        cases = campaign_cases(args)
        self.assertEqual(len(cases), 42)
        self.assertEqual(cases, campaign_cases(parser.parse_args(argv + options)))
        self.assertNotEqual(cases, campaign_cases(parser.parse_args(argv + options + ["--seed", "429"])))
        for repetition in range(1, 4):
            paired = [case for case in cases if case["repetition"] == repetition]
            self.assertEqual({case["body_bytes"] for case in paired}, set(targets))
            for index in range(0, len(paired), 2):
                self.assertEqual(paired[index]["body_bytes"], paired[index + 1]["body_bytes"])
                self.assertEqual({paired[index]["profile"], paired[index + 1]["profile"]}, {"baseline", "fast"})
        for case in cases:
            config = trial_config(args, fixture_config(), case, "fixture-0123456789")
            self.assertEqual((config["headers"], config["solution_bytes"], config["max_body"], config["timing_profile"]),
                             (32, 0, MAX_BENCH_BODY, "wan"))
            self.assertEqual(config["body_bytes"], case["body_bytes"])
            self.assertIn(f"b{case['body_bytes']}-cap{MAX_BENCH_BODY}", trial_name(1, case))
        first = cases[0]
        self.assertNotEqual(trial_name(1, first), trial_name(1, dict(first, max_body=MAX_BODY)))
        self.assertNotEqual(trial_name(1, first), trial_name(1, dict(first, timing_profile="production")))
        for conflict in (["--headers", "32"], ["--solution-bytes", "0"]):
            with self.assertRaises(ValueError):
                campaign_cases(parser.parse_args(argv + options + conflict))
        with self.assertRaises(ValueError):
            campaign_cases(parser.parse_args(argv + ["--body-bytes", "10485760"]))
        for value in ("0", "1.0", "-1", "+1", "1_024", "10MiB", " 10000", "10000,", "10000,10000", "10485761"):
            with self.subTest(value=value), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                parser.parse_args(argv + ["--body-bytes", value])

    def test_caps_targets_timing_and_chronology_do_not_pool(self):
        trials = []
        for index, (target, cap, timing) in enumerate(((0, MAX_BODY, "production"),
                (131072, MAX_BODY, "production"), (131072, MAX_BENCH_BODY, "production"),
                (131072, MAX_BENCH_BODY, "wan"), (524288, MAX_BENCH_BODY, "wan")), 1):
            trial = self.counter_trial(f"trial-{index:03d}", body=target or 100)
            trial["config"].update(body_bytes=target, max_body=cap, timing_profile=timing)
            for node, report in trial["node_reports"].items():
                report["config"] = dict(trial["config"], node=node)
            trial.update(trial_index=index, repetition=1, started_at=f"2026-11-26T18:0{index}:00+00:00",
                         finished_at=f"2026-11-26T18:0{index}:09+00:00", elapsed_seconds=9)
            trials.append(trial)
        rows = aggregate(trials)
        self.assertEqual(len(rows), 5)
        for row in rows:
            self.assertEqual(row["trials"], 1)
            self.assertEqual((row["elapsed_seconds"], row["duration_seconds"], row["elapsed_trial_count"]), (9, 2, 1))
            self.assertEqual((row["planned_measured_heights"], row["observed_measured_heights"], row["uncompleted_measured_heights"]),
                             (1, 1, 0))
            chronology = row["trial_chronology"][0]
            self.assertEqual((chronology["completed_measured_heights"], chronology["rate_duration_seconds"]), (1, 2))
            self.assertEqual(row["started_at"], chronology["started_at"])
            self.assertEqual(row["body_bytes"], row["target_body_bytes"] or 100)
            self.assertEqual(row["measured_window"]["system_delta"]["network.eth0.tx_bytes"]["matched_trials"], 1)
        old = aggregate([self.counter_trial()])[0]
        self.assertIsNone(old["elapsed_seconds"])
        self.assertIsNone(old["started_at"])
        self.assertEqual(old["elapsed_trial_count"], 0)

    def test_new_serde_defaults_only_normalize_for_comparison(self):
        trial = self.counter_trial()
        old = dict(trial["node_reports"][0]["config"])
        new = dict(old, body_bytes=0, timing_profile="production")
        self.assertTrue(configs_match(old, new))
        self.assertTrue(configs_match(new, old))
        trial["node_reports"][0]["config"] = new
        self.assertEqual(aggregate_node_windows([trial])["valid_node_windows"], 4)
        self.assertEqual(trial["config"], old)  # Comparison never fills defaults into original config.
        for change in ({"body_bytes": 10000}, {"body_bytes": False}, {"timing_profile": "wan"},
                       {"max_body": MAX_BENCH_BODY}, {"extra": 0}):
            self.assertFalse(configs_match(dict(new, **change), old))
        missing_old_field = dict(new)
        del missing_old_field["solution_bytes"]
        self.assertFalse(configs_match(missing_old_field, old))
        trial["node_reports"][0]["config"]["max_body"] = MAX_BENCH_BODY
        metric = aggregate_node_windows([trial])["system_delta"]["network.eth0.tx_bytes"]
        self.assertIsNone(metric["total"])
        self.assertEqual(metric["available_node_windows"], 3)
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            root = Path(temporary)
            write_json(root / "config.json", old)
            write_json(root / "manifest.json", {"binary_sha256": "fixture"})
            write_json(root / "worker.json", {"run_id": old["run_id"], "node": 0, "binary_sha256": "fixture", "returncode": 0})
            report = dict(trial["node_reports"][0], config=dict(old, body_bytes=0, timing_profile="production"))
            write_json(root / "report.json", report)
            before = (root / "report.json").read_bytes()
            self.assertEqual(validate_node_artifacts(root, old, "fixture")[1], report)
            self.assertEqual((root / "report.json").read_bytes(), before)

    def test_atomic_artifacts_preserve_create_only_and_raw_text(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            root = Path(temporary)
            path = root / "artifact.json"
            with self.assertRaises(ValueError):
                write_json(path, {"bad": float("nan")})
            self.assertEqual(list(root.iterdir()), [])
            write_json(path, {"keep": "original"})
            original = path.read_bytes()
            with self.assertRaises(FileExistsError):
                write_json(path, {"replacement": "forbidden"})
            self.assertEqual(path.read_bytes(), original)
            self.assertEqual(list(root.iterdir()), [path])
            raw = '{"partial_raw_report":\n'
            identifier = "fixture-0123456789"
            def request(_address, _token, operation, value, **_kwargs):
                self.assertEqual(value, {"run_id": identifier})
                if operation == "status":
                    return {"run_id": identifier, "done": True}
                self.assertEqual(operation, "result")
                return {"run_id": identifier, "files": {"report.json": raw}}
            with mock.patch(__name__ + ".worker_request", side_effect=request):
                collect_remote("[::1]:21000", "not-sent", identifier, root, 0)
            self.assertEqual((root / "report.json").read_text(), raw)

    def test_failure_report_grace_is_shared_and_excluded_from_measurement(self):
        for now, started, expected in ((189, True, 8), (197, True, 8), (180, True, 0),
                                       (199, True, 0), (190, False, 0)):
            self.assertEqual(failure_report_grace(100, 90, now, started), expected)
        roster = {"peers": fixture_config()["peers"], "coordinator": "[::1]:21000", "interface": "lo",
                  "workers": [None, "[::1]:22001", "[::1]:22002", "[::1]:22003"]}
        clock = [100.0]
        def controller(config, _ready_timeout):
            state = Barrier(config, 100)
            for node in range(4):
                state.ready({"type": "ready", "node": node, "run_id": config["run_id"]}, 101)
            state.start(102)
            state.finish(190, "node disconnected before final ACK")
            result = mock.Mock(state=state)
            result.done.is_set.return_value = True
            def wait(_seconds):
                clock[0] = 190
                return True
            result.done.wait.side_effect = wait
            result.report.side_effect = lambda: state.report(clock[0])
            return result
        local = mock.Mock(began=100)
        local.done.is_set.return_value = True
        def local_wait(seconds):
            self.assertEqual(seconds, 8)
            clock[0] += 3
            return True
        local.done.wait.side_effect = local_wait
        waits = []
        def collect(_address, _token, _identifier, _directory, grace):
            waits.append(grace)
            clock[0] += 1
        def request(_address, _token, operation, value):
            self.assertEqual(operation, "start")
            return {"accepted": True, "node": value["node"], "run_id": value["run_id"]}
        args = build_parser().parse_args(["campaign", "--binary", sys.executable, "--roster", "unused",
            "--token-file", "unused", "--out", "unused", "--timeout-secs", "90", "--warmup", "0"])
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary, \
                mock.patch(__name__ + ".time.monotonic", side_effect=lambda: clock[0]), \
                mock.patch(__name__ + ".Coordinator", side_effect=controller), \
                mock.patch(__name__ + ".NodeJob", return_value=local), \
                mock.patch(__name__ + ".worker_request", side_effect=request), \
                mock.patch(__name__ + ".collect_remote", side_effect=collect), \
                mock.patch(__name__ + ".validate_node_artifacts", side_effect=ValueError("partial fixture")):
            result = run_trial(args, roster, "not-sent", Path(sys.executable), {"binary_sha256": "fixture"},
                               Path(temporary) / "trial", {"headers": 32, "profile": "baseline", "repetition": 1}, 300)
        self.assertEqual(waits, [5, 4, 3])
        self.assertEqual(result["status"], "partial")
        self.assertEqual(result["controller"]["rate_duration_seconds"], 88)
        self.assertEqual(result["elapsed_seconds"], 96)
        self.assertEqual(result["report_grace_seconds"], 8)
        self.assertEqual(result["cleanup_unconfirmed_nodes"], [])
        local.cancel.assert_not_called()


class RealTests(unittest.TestCase):
    binary = None

    def test_four_real_nodes(self):
        self.run_local_nodes(4, ["--headers", "32", "--profiles", "baseline,fast_k1,economy",
                                 "--heights", "3", "--timeout-secs", "30"])

    def test_seven_real_nodes_exact_10mib_wan(self):
        self.run_local_nodes(7, ["--body-bytes", "10485760", "--max-body", "10485760",
                                 "--timing-profile", "wan", "--heights", "2", "--timeout-secs", "240"])

    def run_local_nodes(self, nodes, options):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            root = Path(temporary)
            token_file = root / "token"
            create_token(token_file)
            token = read_token(token_file)
            servers, threads, workers = [], [], []
            try:
                for node in range(1, nodes):
                    worker = Worker(self.binary, root / f"worker-{node}", "lo")
                    server = WorkerServer(("::1", 0), WorkerHandler)
                    server.worker, server.token = worker, token
                    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True)
                    thread.start()
                    servers.append(server); threads.append(thread); workers.append(worker)
                # Hold distinct UDP ports while allocating; Rust owns them after release.
                with contextlib.ExitStack() as stack:
                    ports = []
                    for _ in range(nodes):
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
                    "--out", str(root / "smoke"), "--repetitions", "1", "--warmup", "1"] + options)
                result = campaign(args)
                details = {str(path.relative_to(root)): optional_text(path) for pattern in
                           ("trial-*/result.json", "trial-*/controller.json", "trial-*/node-*/stderr.txt")
                           for path in (root / "smoke").glob(pattern)}
                self.assertEqual(result, 0, details)
                summaries = read_json(root / "smoke" / "summary.json")["cases"]
                self.assertEqual([row["profile"] for row in summaries], sorted(args.profiles))
                for summary in summaries:
                    with self.subTest(profile=summary["profile"]):
                        self.assertEqual(summary["completed_heights"], args.heights)
                        self.assertGreater(summary["body_bytes"], 18000)
                        self.assertEqual(summary["receipt_ms"]["all"]["n"], args.heights)
                        if args.body_bytes:
                            self.assertEqual(summary["body_sizes"], args.body_bytes)
                            self.assertEqual(summary["body_bytes"], args.heights * args.body_bytes[0])
                            self.assertEqual(summary["timing_profile"], "wan")
                            self.assertEqual(summary["measured_window"]["valid_node_windows"], nodes)
                for trial in (root / "smoke").glob("trial-*"):
                    profile = read_json(trial / "trial.json")["profile"]
                    reports = list(trial.glob("node-*/report.json"))
                    self.assertEqual(len(reports), nodes)
                    for path in reports:
                        report = read_json(path)
                        config = report["config"]
                        self.assertEqual(config["max_requests"], PROFILES[profile]["max_requests"])
                        self.assertEqual(config["max_outgoing"], 2)
                        if args.body_bytes:
                            self.assertEqual(config["body_bytes"], args.body_bytes[0])
                            self.assertEqual({row["body_bytes"] for row in report["heights"]}, set(args.body_bytes))
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
        raise ValueError("profile must be one of: " + ", ".join(PROFILES))
    return text


class ExplicitStore(argparse.Action):
    def __call__(self, parser, namespace, value, option_string=None):
        setattr(namespace, self.dest, value)
        setattr(namespace, self.dest + "_explicit", True)


def body_bytes_arg(text):
    if re.fullmatch(r"[0-9]+", text) is None:
        raise argparse.ArgumentTypeError("body bytes must be positive decimal integers (no unit suffixes)")
    return bounded_arg(1, MAX_BENCH_BODY)(text)


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
    command.set_defaults(headers_explicit=False, solution_bytes_explicit=False)
    command.add_argument("--headers", type=comma_arg(bounded_arg(0, 2048)), default=[32, 128, 512, 2048],
                         action=ExplicitStore)
    command.add_argument("--solution-bytes", type=bounded_arg(0, 1344), default=0, action=ExplicitStore,
                         help="applies to all sizes; near-cap arm: --headers 2048 --solution-bytes 320")
    command.add_argument("--body-bytes", type=comma_arg(body_bytes_arg),
                         help="exact serialized body targets, comma-separated decimal bytes; excludes --headers/--solution-bytes")
    command.add_argument("--max-body", type=bounded_arg(1, MAX_BENCH_BODY), default=MAX_BODY,
                         help="fixed body cap across the sweep, bytes (default 1048576; maximum 10485760)")
    command.add_argument("--timing-profile", choices=("production", "wan"), default="production",
                         help="benchmark-only protocol timing profile; fixed across the sweep")
    command.add_argument("--profiles", type=comma_arg(profile_arg), default=["baseline"],
                         help="comma-separated baseline,fast,fast_k1,economy; economy polls at 100ms; never changes protocol timers")
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
