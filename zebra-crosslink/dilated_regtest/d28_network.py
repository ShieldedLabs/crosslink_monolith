#!/usr/bin/env python3
"""Isolated real-zebrad D28 acceptance network (Python 3.11+, standard library).

  python3 d28_network.py selftest
  python3 d28_network.py check --binary /absolute/path/to/zebrad
  python3 d28_network.py run --binary /absolute/path/to/zebrad --out /NEW/path \
      --late-finalizer --offline-finalizer --crash-restart

run enters a NEW rootless Linux user+network namespace, enables only its loopback,
and runs all nodes and RPC clients there. No host-network fallback or root is used.
STP binds wildcard UDP internally, so loopback peer strings alone are NOT isolation.
check and selftest never start nodes, enter namespaces, or bind sockets.

Every run owns a new private directory, preserved on success AND failure. No existing
chain, config, process, or output directory is removed. Each node has fresh state,
identity, ports and boot logs. Only node 0 mines (generate RPC); its wallet funds
four equal finalizers using the existing faucet/staking RPCs. Peers form a static
full mesh. The short calendar and 90x clock come from node0.toml.in. Existing
FeaturenetUpgrade2 rules activate at height 1 to exercise the certificate gate.

Default: all nodes make progress, then one finalizer restarts on the same state.
--late-finalizer withholds the last node until the other three decide; its only
previous boot discovered its identity at genesis. --offline-finalizer also requires
progress while that node is stopped. Three of four equal stakes exceed two thirds.
No simulated votes, mocked ledger, modified consensus rules, or external peers.

Samples compare certified BFT hashes at equal heights (not signature subsets),
finalized PoW hashes, and accepted common PoW prefixes. They retain full certificate
RPC replies, but do not independently verify Ed25519 in Python: acceptance is by the
real nodes. Template certificate + accepted post-activation blocks exercise the
chain gate. Startup logs must contain --require-log (default D28); a legacy runtime
must not accidentally pass. This finite run is not a liveness or memory proof.

A native debug build can need tens of minutes, including cold shielded-key/prover
initialization. Default wall budget is one hour; release-iter is faster. Native
build: cargo build --manifest-path zebra-crosslink/Cargo.toml --locked -p zebrad
--bin zebrad [--profile release-iter]. No build is performed here.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from unittest import mock
import urllib.error
import urllib.request

HERE = Path(__file__).resolve().parent
STAKING, ROSTER, ACTIVATION = 150, 300, 450
BOND = 10_000_000
FINALIZER = re.compile(r"finalizer address: (zfinv1[A-Za-z0-9_-]+)")
IDENTITY = re.compile(r"NewNet: KEYPAIR: magic1: (\S+) sk: REDACTED pk: (\S+)")
# Do not inherit HTTP proxies for loopback RPC, even inside the isolated namespace.
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))


class Failure(Exception):
    pass


class RpcError(Failure):
    pass


def require(condition, message):
    if not condition:
        raise Failure(message)


def say(message):
    print(f"[{time.strftime('%H:%M:%S')}] {message}", flush=True)


def dump(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def new_output(path):
    if path is None:
        return Path(tempfile.mkdtemp(prefix="d28-network-")).resolve()
    # lexists also refuses a dangling symlink. mkdir(exist_ok=False) closes the race.
    require(not os.path.lexists(path), f"output already exists: {path}")
    require(path.parent.is_dir(), f"output parent must exist: {path.parent}")
    path.mkdir(mode=0o700, exist_ok=False)
    return path.resolve()


def clean_env(home):
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("ZEBRA_", "NEXTEST_", "CROSSLINK_"))
           and k not in ("SENTRY_DSN", "DISPLAY", "WAYLAND_DISPLAY")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_CACHE_HOME=str(home / "cache"), XDG_DATA_HOME=str(home / "data"),
               RUST_BACKTRACE="1")
    return env


def set_key(text, section, key, value):
    """Change only a named TOML section; JSON literals are valid for these TOML values."""
    lines = text.splitlines()
    header = f"[{section}]"
    starts = [i for i, line in enumerate(lines) if line.strip() == header]
    require(len(starts) <= 1, f"duplicate TOML section: {section}")
    entry = f"{key} = {json.dumps(value)}"
    if not starts:
        return text.rstrip() + f"\n\n{header}\n{entry}\n"
    start = starts[0]
    end = next((i for i in range(start + 1, len(lines))
                if lines[i].lstrip().startswith("[")), len(lines))
    found = [i for i in range(start + 1, end)
             if re.match(rf"^\s*{re.escape(key)}\s*=", lines[i])]
    require(len(found) <= 1, f"duplicate TOML key: {section}.{key}")
    if found:
        lines[found[0]] = entry
    else:
        lines.insert(end, entry)
    return "\n".join(lines) + "\n"


def ports(base, index):
    # Namespace is fresh, so deterministic distinct ports need no bind-and-release race.
    return dict(zip(("rpc", "p2p", "stp", "grpc", "ready"),
                    range(base + 10 * index, base + 10 * index + 5)))


def render(template, root, index, args, peers, discovery=False):
    node = root / f"node{index}"
    p = ports(args.port_base, index)
    text = template.replace("@ROOT@", str(root)).replace("@START@", str(args.start_unix))
    settings = {
        "debug_time_dilation": {"multiplier": args.dilation},
        "crosslink": {
            "public_address": f"127.0.0.1:{p['stp']}",
            "explicit_bft_key_seed": f"{args.seed}:finalizer:{index}",
            "bft_peers": [], "disable_shipped_slashforks": True,
            "disable_the_headless_wallet": discovery or index != 0,
            "public_lightwallet_server": False,
            "lightwallet_server_listen_addr": f"[::1]:{p['grpc']}",
            "lightwallet_server_ready_port": p["ready"],
        },
        "network": {
            "network": "Regtest", "cache_dir": False,
            "initial_mainnet_peers": [], "initial_testnet_peers": [],
            "listen_addr": f"127.0.0.1:{p['p2p']}",
        },
        "network.testnet_parameters.activation_heights": {"FeaturenetUpgrade2": 1},
        "rpc": {"listen_addr": f"127.0.0.1:{p['rpc']}", "enable_cookie_auth": False,
                "cookie_dir": str(node / "state")},
        "state": {"cache_dir": str(node / "state"), "ephemeral": False,
                  "delete_old_database": False,
                  "network_identity_seed_string": f"{args.seed}:transport:{index}",
                  "network_local_port": p["stp"], "network_initial_peers": peers,
                  "network_connect_only": True},
        "state.network_checkpoint": {"height": 0, "hash": ""},
        "mining": {"internal_miner": False},
        "tracing": {"use_color": False},
    }
    for section, fields in settings.items():
        for key, value in fields.items():
            text = set_key(text, section, key, value)
    cfg = tomllib.loads(text)
    require(cfg["network"]["testnet_parameters"]["crosslink"] == {
        "bootstrap_staking_height": STAKING, "bootstrap_roster_height": ROSTER,
        "bootstrap_activation_height": ACTIVATION, "staking_period": 150,
        "staking_day_window": 70, "staking_action_delay": 75,
    }, "template short staking calendar changed; review the harness before running")
    require("@ROOT@" not in text and "@START@" not in text, "unrendered template")
    require("dilated_regtest/out/" not in text, "shared output path survived rendering")
    for peer in peers:
        require(re.fullmatch(r"\[::1\]:[0-9]+:[A-Za-z0-9_-]+:[A-Za-z0-9_-]+", peer),
                f"not a loopback STP peer: {peer}")
    return text


def certificate(pointer):
    if pointer is None:
        return {"height": 0, "hash": None, "round": 0, "signers": 0}
    raw = pointer.get("vote_for_block_without_finalizer_public_key")
    require(isinstance(raw, list) and len(raw) == 44
            and all(type(b) is int and 0 <= b <= 255 for b in raw), "malformed certificate vote")
    raw = bytes(raw)
    height = int.from_bytes(raw[32:40], "little")
    round_bits = int.from_bytes(raw[40:44], "little")
    signatures = pointer.get("signatures")
    require(isinstance(signatures, list), "missing certificate signatures")
    if height:
        require(round_bits & 0x80000000 and signatures, "non-genesis certificate has no commit quorum")
    signers = set()
    for sig in signatures:
        key = json.dumps(sig["pub_key"], sort_keys=True)
        require(key not in signers, "duplicate certificate signer")
        signers.add(key)
        value = sig["vote_signature"]
        require(isinstance(value, list) and len(value) == 64
                and all(type(b) is int and 0 <= b <= 255 for b in value), "malformed signature")
    return {"height": height, "hash": raw[:32].hex(),
            "round": round_bits & 0x7fffffff, "signers": len(signers)}


def remember_decision(seen, cert):
    if cert["height"]:
        old = seen.setdefault(cert["height"], cert["hash"])
        require(old == cert["hash"], f"conflicting certified hashes at BFT {cert['height']}")


class Node:
    def __init__(self, index, root, args, deadline):
        self.index, self.args, self.deadline = index, args, deadline
        self.path = root / f"node{index}"
        self.path.mkdir()
        (self.path / "home").mkdir()
        self.config = self.path / "node.toml"
        self.proc = None
        self.boot = 0
        self.logs = []
        self.exits = []
        self.rpc_port = ports(args.port_base, index)["rpc"]
        self.finalizer = self.peer = None

    def start(self, config):
        require(self.proc is None or self.proc.poll() is not None, "node is already running")
        self.config.write_text(config)
        self.boot += 1
        log = self.path / f"boot{self.boot}.log"
        self.logs.append(log)
        # Config snapshots and separate boot logs survive every restart.
        (self.path / f"boot{self.boot}.toml").write_text(config)
        with log.open("xb") as output:
            self.proc = subprocess.Popen(
                [str(self.args.binary), "-c", str(self.config), "start", "--headless"],
                cwd=self.path, env=clean_env(self.path / "home"), stdin=subprocess.DEVNULL,
                stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        say(f"node{self.index} boot{self.boot} pid={self.proc.pid} rpc={self.rpc_port}")

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    def text(self, all_boots=False):
        return "\n".join(p.read_text(errors="replace") for p in (self.logs if all_boots else self.logs[-1:]))

    def rpc(self, method, params=None, timeout=30):
        require(self.alive(), f"node{self.index} exited; see {self.logs[-1]}")
        left = self.deadline - time.monotonic()
        require(left > 0, "overall wall-clock timeout")
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method,
                           "params": params or []}).encode()
        request = urllib.request.Request(f"http://127.0.0.1:{self.rpc_port}", body,
                                         {"content-type": "application/json"})
        try:
            with HTTP.open(request, timeout=min(timeout, left)) as response:
                reply = json.load(response)
        except urllib.error.HTTPError as error:
            reply = json.loads(error.read())
        if reply.get("error") is not None:
            raise RpcError(f"node{self.index} {method}: {reply['error']}")
        require("result" in reply, f"node{self.index} {method}: no RPC result")
        return reply["result"]

    def ready(self):
        end = min(self.deadline, time.monotonic() + self.args.startup_timeout)
        last = None
        while time.monotonic() < end:
            require(self.alive(), f"node{self.index} startup failed: {self.logs[-1]}")
            text = self.text()
            fin, identity = FINALIZER.search(text), IDENTITY.search(text)
            try:
                height = self.rpc("getblockcount", timeout=2)
            except (OSError, ValueError, RpcError) as error:
                last = str(error)
            else:
                if fin and identity:
                    address = fin.group(1)
                    peer = f"[::1]:{ports(self.args.port_base, self.index)['stp']}:{identity[1]}:{identity[2]}"
                    require(self.finalizer in (None, address), "finalizer identity changed on restart")
                    require(self.peer in (None, peer), "transport identity changed on restart")
                    self.finalizer, self.peer = address, peer
                    return height
            time.sleep(0.2)
        raise Failure(f"node{self.index} startup timed out ({last}); see {self.logs[-1]}")

    def stop(self, crash=False, strict=True):
        proc = self.proc
        if proc is None:
            return
        # poll() reaps; never signal a reaped PID (which may since have been reused).
        running = proc.poll() is None
        forced = False
        if running:
            try:
                os.killpg(proc.pid, signal.SIGKILL if crash else signal.SIGINT)
            except ProcessLookupError:
                pass
            try:
                proc.wait(timeout=self.args.shutdown_grace)
            except subprocess.TimeoutExpired:
                forced = True
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait(timeout=10)
        self.proc = None
        self.exits.append({"boot": self.boot, "code": proc.returncode,
                           "intentional_crash": crash, "forced": forced})
        if strict:
            require(running, f"node{self.index} had already exited ({proc.returncode})")
            require(not forced and (crash or proc.returncode == 0),
                    f"node{self.index} shutdown failed: {self.exits[-1]}")


class Network:
    def __init__(self, args, root, template):
        self.args, self.root, self.template = args, root, template
        self.deadline = time.monotonic() + args.timeout
        self.nodes = [Node(i, root, args, self.deadline) for i in range(args.nodes)]
        self.source = self.nodes[0]
        self.seen = {}
        self.samples = 0
        self.last = []
        self.certified_template = None

    def config(self, node, discovery=False):
        peers = [] if discovery else [n.peer for n in self.nodes if n is not node]
        return render(self.template, self.root, node.index, self.args, peers, discovery)

    def active(self):
        return [n for n in self.nodes if n.proc is not None]

    def wait(self, what, predicate, mine=False, max_height=None):
        end = min(self.deadline, time.monotonic() + self.args.phase_timeout)
        last = None
        while time.monotonic() < end:
            for node in self.active():
                require(node.alive(), f"node{node.index} exited: {node.logs[-1]}")
            try:
                result = predicate()
            except (OSError, ValueError, RpcError) as error:
                last = str(error)
            else:
                if result:
                    return result
            if mine:
                if max_height is not None:
                    require(self.source.rpc("getblockcount") < max_height,
                            f"{what} did not finish before height {max_height}")
                self.mine()
            time.sleep(0.3)
        raise Failure(f"timed out: {what}; last RPC error: {last}")

    def mine(self):
        hashes = self.source.rpc("generate", [1], timeout=self.args.phase_timeout)
        require(isinstance(hashes, list) and len(hashes) == 1, "generate did not return one block")

    def mine_to(self, height):
        while self.source.rpc("getblockcount") < height:
            self.mine()
            if self.source.rpc("getblockcount") % self.args.sample_every == 0:
                self.sample("mining")

    def sample(self, phase):
        rows = []
        for node in self.active():
            pointer = node.rpc("get_tfl_fat_pointer_to_bft_chain_tip")
            cert = certificate(pointer)
            remember_decision(self.seen, cert)
            final = node.rpc("get_tfl_final_block_height_and_hash")
            # A catching-up node can advance between RPCs. Read its tip AFTER finality so
            # a newly imported decision is not compared to a stale, lower tip sample.
            height = node.rpc("getblockcount")
            if final:
                require(final["height"] <= height, "finalized height exceeds PoW tip")
                expected = self.source.rpc("getblockhash", [final["height"]])
                require(final["hash"] == expected, f"node{node.index} finalized a different PoW chain")
            rows.append({"node": node.index, "pow_height": height, "final": final,
                         "certificate": cert, "pointer": pointer})
        if rows:
            common = min(r["pow_height"] for r in rows)
            hashes = [n.rpc("getblockhash", [common]) for n in self.active()]
            require(len(set(hashes)) == 1, f"nodes disagree at PoW height {common}")
        self.samples += 1
        self.last = rows
        with (self.root / "samples.jsonl").open("a") as output:
            output.write(json.dumps({"time": time.time(), "phase": phase, "nodes": rows}) + "\n")
        say(phase + " " + " ".join(f"n{r['node']}:pow={r['pow_height']} bft={r['certificate']['height']} fin={r['final']['height'] if r['final'] else '-'}" for r in rows))
        return rows

    def progress(self, phase, min_pow, min_bft):
        # Keep mining during BFT/catch-up waits; an idle tip can force endless nil rounds.
        self.mine_to(min_pow)
        def reached():
            rows = self.sample(phase)
            return all(r["certificate"]["height"] >= min_bft and r["final"]
                       and r["pow_height"] - r["final"]["height"] <= self.args.max_finality_lag
                       for r in rows)
        self.wait(phase, reached, mine=True)
        return max(r["certificate"]["height"] for r in self.last)

    def fund_and_bond(self):
        wallet = self.wait("wallet's first sync", lambda: self.source.rpc("wallet_spendable_funds"))
        self.mine_to(4)
        # One faucet payment is 0.5 cTAZ; support 4..8 nodes without changing bond sizes.
        for _ in range((self.args.nodes + 3) // 4):
            before = self.source.rpc("wallet_spendable_funds")["spendable_zats"]
            self.source.rpc("requestfaucetdonation", [{"address": wallet["address"]}],
                            timeout=self.args.phase_timeout)
            self.wait("faucet transaction in mempool", lambda: self.source.rpc("getrawmempool"))
            self.wait("faucet funds mature", lambda: self.source.rpc("wallet_spendable_funds")["spendable_zats"] > before,
                      mine=True, max_height=STAKING - 1)
        self.wait("funds for all bonds and fees", lambda: self.source.rpc("wallet_spendable_funds")["spendable_zats"] >= self.args.nodes * BOND + 1_000_000)
        self.mine_to(STAKING)
        for node in self.nodes:
            before = self.source.rpc("wallet_staking_positions")["active"]
            count = sum(len(v) for v in before.values())
            command = {"CreateNewDelegationBond": {"amount_zats": BOND,
                                                    "target_finalizer": node.finalizer}}
            self.source.rpc("wallet_staking_action", [command], timeout=self.args.phase_timeout)
            self.wait(f"node{node.index} bond enters mempool", lambda: self.source.rpc("getrawmempool"))
            self.wait(f"node{node.index} bond confirmed", lambda: sum(len(v) for v in self.source.rpc("wallet_staking_positions")["active"].values()) > count,
                      mine=True, max_height=STAKING + 65)
            # Wait for the change note to mature before the next bond, not just any mempool tx.
            left = self.args.nodes - node.index - 1
            if left:
                self.wait("bond change spendable", lambda: self.source.rpc("wallet_spendable_funds")["spendable_zats"] >= left * BOND + 1_000_000,
                          mine=True, max_height=STAKING + 65)
        require(self.source.rpc("getblockcount") < ROSTER, "bonds missed bootstrap roster snapshot")
        dump(self.root / "bonds.json", self.source.rpc("wallet_staking_positions"))

    def run(self):
        # No blocks are mined in discovery; the late finalizer's state stays at genesis.
        for node in self.nodes:
            node.start(self.config(node, discovery=True))
            require(node.ready() == 0, "discovery unexpectedly mined blocks")
            node.stop()
        dump(self.root / "identities.json", [{"node": n.index, "finalizer": n.finalizer,
                                               "peer": n.peer} for n in self.nodes])
        for node in self.nodes:
            if not (self.args.late_finalizer and node is self.nodes[-1]):
                node.start(self.config(node))
                node.ready()
        self.fund_and_bond()
        first = self.progress("initial progress", ACTIVATION + 30, 3)
        for node in self.active():
            require(re.search(self.args.require_log, node.text()),
                    f"node{node.index} did not start the required D28 runtime")
            info = node.rpc("getblockchaininfo")
            gate = info.get("upgrades", {}).get("46545532", {})
            require(gate.get("status") == "active" and gate.get("activationheight") == 1,
                    f"node{node.index} has no active FeaturenetUpgrade2 certificate gate")
            dump(node.path / "blockchain-info.json", info)
            roster = node.rpc("get_tfl_roster_with_addresses")
            require(isinstance(roster, list) and len(roster) == self.args.nodes,
                    f"node{node.index} roster does not contain every bonded finalizer")
            weights = [member["voting_power"] for member in roster]
            require(all(weight > 0 for weight in weights)
                    and 3 * (sum(weights) - max(weights)) > 2 * sum(weights),
                    "removing one finalizer would leave no strict quorum")
            dump(node.path / "roster.json", roster)
        if self.args.late_finalizer:
            node = self.nodes[-1]
            node.start(self.config(node))
            node.ready()
            first = self.progress("late finalizer catches up", self.source.rpc("getblockcount") + 20, first + 2)
        victim = self.nodes[-1]
        before = certificate(victim.rpc("get_tfl_fat_pointer_to_bft_chain_tip"))["height"]
        victim.stop(crash=self.args.crash_restart)
        if self.args.offline_finalizer:
            first = self.progress("quorum progresses with one offline", self.source.rpc("getblockcount") + 20, first + 2)
        victim.start(self.config(victim))
        victim.ready()
        require("crosslink bootstrap: PoW reached height" not in victim.text(),
                "restarted finalizer bootstrapped again instead of restoring")
        self.progress("restarted finalizer recovers", self.source.rpc("getblockcount") + 20,
                      max(before, first) + 2)
        self.progress("final progress", max(self.args.target, self.source.rpc("getblockcount") + 10),
                      max(r["certificate"]["height"] for r in self.last) + 2)
        # Capture a non-genesis certificate selected by the real mining template. Then require
        # further PoW accepted by every node, with FeaturenetUpgrade2's certificate gate active.
        template = self.source.rpc("getblocktemplate", [{}], timeout=self.args.phase_timeout)
        self.certified_template = certificate(template["fat_pointer_to_bft_block"])
        require(self.certified_template["height"] > 0, "mining template has no BFT certificate")
        dump(self.root / "certified-template.json", template)
        height = self.source.rpc("getblockcount") + 2
        self.mine_to(height)
        self.wait("all peers accept certified PoW", lambda: all(n.rpc("getblockcount") >= height for n in self.active()))
        self.sample("accepted certified PoW")
        # Compare EVERY accepted PoW hash, not just reported tip heights.
        reference = [self.source.rpc("getblockhash", [h]) for h in range(height + 1)]
        for node in self.nodes[1:]:
            require([node.rpc("getblockhash", [h]) for h in range(height + 1)] == reference,
                    f"node{node.index} differs on the accepted PoW prefix")
        dump(self.root / "pow-prefix.json", reference)
        for node in self.nodes:
            require(re.search(self.args.require_log, node.text(all_boots=True)),
                    f"node{node.index} never logged runtime marker {self.args.require_log!r}")
        for node in self.nodes:
            node.stop()
            text = node.text(all_boots=True)
            require("panicked" not in text and "empty roster" not in text,
                    f"node{node.index} panic or empty roster")
            require(not re.search(r"\bERROR\b", text), f"node{node.index} logged ERROR; inspect preserved logs")
        require(not any(self.root.rglob("pos.chain")), "obsolete pos.chain store appeared")
        return {"status": "PASS", "samples": self.samples, "pow_height": height,
                "certified_template": self.certified_template,
                "observed_bft_heights": len(self.seen), "final": self.last}


def netns_id():
    return os.readlink("/proc/self/ns/net")


def enter_namespace(args):
    if args.parent_netns is None:
        argv = [shutil.which("unshare"), "--user", "--map-root-user", "--net", "--",
                sys.executable, str(Path(__file__).resolve()), *sys.argv[1:],
                "--_parent-netns", netns_id()]
        say("entering a fresh rootless network namespace (failure is fatal; no host fallback)")
        os.execv(argv[0], argv)
    require(netns_id() != args.parent_netns, "refusing the host network namespace")
    # sysfs can remain mounted for the parent's network namespace; query this namespace's
    # netlink inventory instead. This does not create sockets that listen or contact peers.
    interfaces = json.loads(subprocess.check_output([shutil.which("ip"), "-json", "link", "show"]))
    require({link["ifname"] for link in interfaces} == {"lo"},
            "namespace has an interface other than loopback")
    subprocess.run([shutil.which("ip"), "link", "set", "lo", "up"], check=True)


def preflight(args):
    require(sys.platform == "linux", "this harness requires Linux rootless network namespaces")
    for tool in ("unshare", "ip"):
        require(shutil.which(tool), f"missing prerequisite: {tool}")
    require(args.binary is not None, "--binary is required for check/run")
    args.binary = args.binary.expanduser().resolve(strict=True)
    require(args.binary.is_file() and os.access(args.binary, os.X_OK), "binary is not executable")
    require(4 <= args.nodes <= 8, "--nodes must be 4..8 (a stopped finalizer must leave >2/3 stake)")
    require(1024 <= args.port_base and args.port_base + args.nodes * 10 <= 65535, "port range invalid")
    require(args.target >= ACTIVATION + 80, f"--target must be >= {ACTIVATION + 80}")
    require(args.dilation >= 1, "--dilation must be positive")
    require(all(x > 0 for x in (args.timeout, args.phase_timeout, args.startup_timeout,
                               args.shutdown_grace, args.sample_every, args.max_finality_lag)), "budgets must be positive")
    require(bool(args.require_log), "--require-log must identify the D28 runtime")
    re.compile(args.require_log)
    if args.out is not None:
        args.out = args.out.expanduser().absolute()
        require(not os.path.lexists(args.out), f"output already exists: {args.out}")
        require(args.out.parent.is_dir(), "--out parent must exist")
    args.start_unix = int(time.time())
    template = (HERE / "node0.toml.in").read_text()
    render(template, args.out or Path("/NEW/d28-check"), 0, args, [], discovery=True)
    return template


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=("check", "run", "selftest"))
    parser.add_argument("--binary", type=Path, help="freshly built native zebrad; never builds or installs")
    parser.add_argument("--out", type=Path, help="NEW directory (parent must exist); default new system-temp directory")
    parser.add_argument("--nodes", type=int, default=4, help="4..8 equal-stake finalizers (default 4)")
    parser.add_argument("--port-base", type=int, default=28000, help="five ports/node, stride 10, inside NEW namespace")
    parser.add_argument("--seed", default="d28-local", help="reproducible transport/finalizer identity seed prefix")
    parser.add_argument("--target", type=int, default=650, help="minimum final PoW height; may mine beyond it for catch-up")
    parser.add_argument("--dilation", type=int, default=90)
    parser.add_argument("--timeout", type=int, default=3600, help="whole network wall-clock budget (seconds)")
    parser.add_argument("--phase-timeout", type=int, default=600, help="individual readiness/progress/funding budget")
    parser.add_argument("--startup-timeout", type=int, default=180)
    parser.add_argument("--shutdown-grace", type=int, default=30)
    parser.add_argument("--sample-every", type=int, default=10, help="PoW blocks between mining samples")
    parser.add_argument("--max-finality-lag", type=int, default=40)
    parser.add_argument("--require-log", default="D28", help="required D28 startup log regex, on EVERY node")
    parser.add_argument("--late-finalizer", action="store_true")
    parser.add_argument("--offline-finalizer", action="store_true")
    parser.add_argument("--crash-restart", action="store_true", help="SIGKILL only the owned finalizer at the restart checkpoint")
    parser.add_argument("--_parent-netns", dest="parent_netns", help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def selftest():
    class Tests(unittest.TestCase):
        def test_render(self):
            args = parse_args(["run", "--binary", "/not/executed"])
            args.start_unix = 123456
            template = (HERE / "node0.toml.in").read_text()
            all_ports = set()
            for i in range(args.nodes):
                p = ports(args.port_base, i)
                self.assertFalse(all_ports.intersection(p.values()))
                all_ports.update(p.values())
                cfg = tomllib.loads(render(template, Path('/NEW/space and "quote'), i, args,
                                          ["[::1]:28002:magic:key"]))
                self.assertEqual(cfg["state"]["cache_dir"], f'/NEW/space and "quote/node{i}/state')
                self.assertFalse(cfg["network"]["cache_dir"])
                self.assertFalse(cfg["network"]["initial_testnet_peers"])
                self.assertTrue(cfg["state"]["network_connect_only"])
                self.assertFalse(cfg["mining"]["internal_miner"])
                self.assertEqual(cfg["crosslink"]["disable_the_headless_wallet"], i != 0)
                self.assertEqual(cfg["network"]["testnet_parameters"]["activation_heights"]["FeaturenetUpgrade2"], 1)
            with self.assertRaises(Failure):
                render(template, Path("/NEW"), 0, args, ["[2001:db8::1]:1:magic:key"])

        def test_sections(self):
            text = '[a]\nport = 1\n[b]\nport = 2\n'
            cfg = tomllib.loads(set_key(text, "a", "port", 3))
            self.assertEqual(cfg, {"a": {"port": 3}, "b": {"port": 2}})
            with self.assertRaises(Failure):
                set_key('[a]\nx=1\nx=2\n', "a", "x", 3)

        def test_output_refusal(self):
            with tempfile.TemporaryDirectory(prefix="d28-selftest-") as temp:
                root = Path(temp)
                out = new_output(root / "new")
                sentinel = out / "keep"
                sentinel.write_text("unchanged")
                with self.assertRaises(Failure):
                    new_output(out)
                self.assertEqual(sentinel.read_text(), "unchanged")
                link = root / "dangling"
                link.symlink_to(root / "missing")
                with self.assertRaises(Failure):
                    new_output(link)

        def test_certificate(self):
            raw = list(bytes([7]) * 32 + (9).to_bytes(8, "little") + (0x80000003).to_bytes(4, "little"))
            sig = {"pub_key": [1] * 32, "vote_signature": [2] * 64}
            pointer = {"vote_for_block_without_finalizer_public_key": raw, "signatures": [sig]}
            cert = certificate(pointer)
            self.assertEqual((cert["height"], cert["round"], cert["signers"]), (9, 3, 1))
            seen = {}
            remember_decision(seen, cert)
            remember_decision(seen, dict(cert, signers=2))  # different valid signer subsets are allowed
            with self.assertRaises(Failure):
                remember_decision(seen, dict(cert, hash="different"))
            with self.assertRaises(Failure):
                certificate(dict(pointer, signatures=[sig, sig]))
            with self.assertRaises(Failure):
                certificate(dict(pointer, signatures=[]))
            with self.assertRaises(Failure):
                certificate(dict(pointer, vote_for_block_without_finalizer_public_key=raw[:43]))

        def test_environment(self):
            with mock.patch.dict(os.environ, {"ZEBRA_STATE__CACHE_DIR": "/existing", "NEXTEST_TEST_GLOBAL_SLOT": "2",
                                             "SENTRY_DSN": "secret", "DISPLAY": ":0"}):
                env = clean_env(Path("/NEW/home"))
            self.assertNotIn("ZEBRA_STATE__CACHE_DIR", env)
            self.assertNotIn("NEXTEST_TEST_GLOBAL_SLOT", env)
            self.assertNotIn("SENTRY_DSN", env)
            self.assertNotIn("DISPLAY", env)
            self.assertEqual(env["HOME"], "/NEW/home")

        def test_reaped_pid_never_signalled(self):
            with tempfile.TemporaryDirectory(prefix="d28-selftest-") as temp:
                args = parse_args(["run"])
                node = Node(0, Path(temp), args, time.monotonic() + 2)
                node.proc = mock.Mock(returncode=0)
                node.proc.poll.return_value = 0
                with mock.patch("os.killpg") as kill:
                    node.stop(strict=False)
                    node.stop(strict=False)
                    kill.assert_not_called()

        def test_namespace_host_refused(self):
            args = parse_args(["run"])
            args.parent_netns = "same"
            with mock.patch(__name__ + ".netns_id", return_value="same"), mock.patch("subprocess.run") as launch:
                with self.assertRaises(Failure):
                    enter_namespace(args)
                launch.assert_not_called()

        def test_namespace_only_loopback(self):
            args = parse_args(["run"])
            args.parent_netns = "parent"
            with mock.patch(__name__ + ".netns_id", return_value="child"), \
                    mock.patch("shutil.which", return_value="/usr/bin/ip"), \
                    mock.patch("subprocess.check_output") as links, mock.patch("subprocess.run") as launch:
                links.return_value = b'[{"ifname":"lo"},{"ifname":"eth0"}]'
                with self.assertRaises(Failure):
                    enter_namespace(args)
                launch.assert_not_called()
                links.return_value = b'[{"ifname":"lo"}]'
                enter_namespace(args)
                launch.assert_called_once_with(["/usr/bin/ip", "link", "set", "lo", "up"], check=True)

        def test_launch_owns_process_and_preserves_boots(self):
            with tempfile.TemporaryDirectory(prefix="d28-selftest-") as temp:
                args = parse_args(["run", "--binary", "/never/executed"])
                node = Node(0, Path(temp), args, time.monotonic() + 2)
                proc = mock.Mock(pid=123456, returncode=0)
                proc.poll.return_value = None
                with mock.patch("subprocess.Popen", return_value=proc) as launch, mock.patch("os.killpg") as kill:
                    node.start("first config")
                    self.assertEqual(launch.call_args.args[0][-2:], ["start", "--headless"])
                    self.assertTrue(launch.call_args.kwargs["start_new_session"])
                    self.assertEqual(launch.call_args.kwargs["cwd"], node.path)
                    node.stop()
                    node.start("second config")
                    node.stop(crash=True)
                    self.assertEqual(kill.call_args_list, [mock.call(proc.pid, signal.SIGINT),
                                                          mock.call(proc.pid, signal.SIGKILL)])
                self.assertEqual((node.path / "boot1.toml").read_text(), "first config")
                self.assertEqual((node.path / "boot2.toml").read_text(), "second config")
                self.assertTrue((node.path / "boot1.log").exists())
                self.assertTrue((node.path / "boot2.log").exists())

        def test_sample_late_catchup_orders_finality_before_tip(self):
            with tempfile.TemporaryDirectory(prefix="d28-selftest-") as temp:
                args = parse_args(["run"])
                net = Network(args, Path(temp), "unused")
                seen_finality = []
                def rpc(method, params=None):
                    if method == "get_tfl_fat_pointer_to_bft_chain_tip":
                        return None
                    if method == "get_tfl_final_block_height_and_hash":
                        seen_finality.append(True)
                        return {"height": 20, "hash": "same-chain"}
                    if method == "getblockcount":
                        return 30 if seen_finality else 10
                    if method == "getblockhash":
                        return "same-chain"
                    self.fail(f"unexpected RPC {method}")
                fake = mock.Mock(index=0, proc=object())
                fake.rpc.side_effect = rpc
                net.nodes, net.source = [fake], fake
                with mock.patch(__name__ + ".say"):
                    rows = net.sample("catchup-fixture")
                self.assertEqual(rows[0]["pow_height"], 30)
                self.assertTrue((Path(temp) / "samples.jsonl").is_file())

    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests))
    return 0 if result.wasSuccessful() else 1


def main():
    args = parse_args()
    if args.command == "selftest":
        return selftest()
    template = preflight(args)
    if args.command == "check":
        say(f"CHECK: executable={args.binary}; template/config/ports valid; unshare and ip found")
        say("No node, namespace, socket or output created. Namespace permission and binary compatibility are NOT tested.")
        return 0
    enter_namespace(args)
    root = new_output(args.out)
    say(f"evidence directory: {root} (always preserved)")
    with args.binary.open("rb") as binary:
        binary_hash = hashlib.file_digest(binary, "sha256").hexdigest()
    manifest = {"arguments": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
                "binary_sha256": binary_hash,
                "network_namespace": netns_id(), "template_sha256": hashlib.sha256(template.encode()).hexdigest(),
                "started": time.time()}
    dump(root / "manifest.json", manifest)
    network = Network(args, root, template)
    def interrupted(signum, _frame):
        raise Failure(f"interrupted by signal {signum}")
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, interrupted)
    result = {"status": "FAIL"}
    try:
        result = network.run()
    except (Failure, OSError, ValueError, KeyError) as error:
        result["error"] = f"{type(error).__name__}: {error}"
        say(result["error"])
    finally:
        # Ignore repeated interrupts during bounded child cleanup; never abandon live children.
        for sig in (signal.SIGINT, signal.SIGTERM):
            signal.signal(sig, signal.SIG_IGN)
        for node in network.nodes:
            try:
                node.stop(strict=False)
            except (OSError, subprocess.TimeoutExpired) as error:
                result["status"] = "FAIL"
                result.setdefault("cleanup_errors", []).append(str(error))
        result["exits"] = {f"node{n.index}": n.exits for n in network.nodes}
        result["elapsed_seconds"] = time.time() - manifest["started"]
        dump(root / "result.json", result)
        say(f"{result['status']}: evidence preserved at {root}")
    return 0 if result["status"] == "PASS" else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (Failure, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"SETUP FAILED: {error}", file=sys.stderr)
        raise SystemExit(2)
