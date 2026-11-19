#!/usr/bin/env python3
"""Headless regression/system test for issue #300: a wallet holding more notes than one
transaction may spend.

Starts ONE isolated, time-dilated Regtest zebrad (a rendering of node0.toml.in on fresh ports and a
fresh data directory, no peers, no peer cache), pays the wallet the normal 0.5 cTAZ faucet amount
repeatedly, and checks through RPC only:

  1. 20 notes, exactly 10 cTAZ: a 10 cTAZ stake fails (no room for the fee), and says so.
  2. an invalid bond amount (not a power of 10) and a missing bond key are rejected with a bond error.
  3. 21 notes (10.5 cTAZ): the stake fails before any merge, and the error names the per-transaction
     input cap; wallet_spendable_funds reports the cap (max_notes_per_tx) and a max_stake_zats below
     10 cTAZ. Nothing is broadcast behind the caller's back (no automatic retries).
  4. wallet_merge_notes (no params, returns a txid) is mined and confirmed once; the stake attempted
     while the merged note is immature fails with a wait reason and is not retried; after 3
     confirmations the 10 cTAZ stake succeeds, is mined, and shows up in wallet_staking_positions.

Needs a zebrad built with the #300 wallet and RPC changes. Python 3 standard library only. No
network beyond 127.0.0.1, no GUI. Only the data directory and the process this script creates are
ever touched.

    wallet_notes.py --zebrad PATH [--out NEWDIR] [--timeout SECONDS] [--keep]

--out must not exist (it is created, and kept); without it a temporary directory is made, and
removed on success unless --keep. The nodes' logs are in <out>/node0.log and node0.err.
Exit status: 0 pass, 1 test failure, 2 setup error.
"""
import argparse, atexit, json, os, re, shutil, signal, socket, subprocess, sys, tempfile, time
import urllib.error, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ZAT = 100_000_000          # 1 cTAZ
FAUCET_ZATS = ZAT // 2     # the normal faucet payment, 0.5 cTAZ
STAKE_ZATS = 10 * ZAT      # a power of 10, bonds must be
STAKING_HEIGHT = 150       # node0.toml.in: staking actions are illegal below it
ROSTER_HEIGHT = 300        # ... and the bond should land before the roster height

failures = []
proc = None
OUT = None
OWN_OUT = False
DEADLINE = 0.0


def say(msg):
    print(f"{time.strftime('%T')} {msg}", flush=True)


def check(cond, msg):
    if cond:
        say(f"ok:   {msg}")
    else:
        say(f"FAIL: {msg}")
        failures.append(msg)
    return cond


class Abort(Exception):
    pass


def remaining():
    left = DEADLINE - time.time()
    if left <= 0:
        raise Abort("overall timeout reached")
    return left


USED_PORTS = set()


def free_port(offsets=(0,)):
    # The node derives its lightwallet ports as p2p+10000 and p2p+10001.
    # Check those too, and avoid a high ephemeral port overflowing the node's u16 addition.
    for _ in range(100):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            p = s.getsockname()[1]
        ports = [p + offset for offset in offsets]
        if max(ports) > 65535 or USED_PORTS.intersection(ports):
            continue
        sockets = []
        try:
            for port in ports:
                s = socket.socket()
                sockets.append(s)
                s.bind(("127.0.0.1", port))
        except OSError:
            continue
        finally:
            for s in sockets:
                s.close()
        USED_PORTS.update(ports)
        return p
    raise Abort("Could not allocate isolated node ports")


class RpcError(Exception):
    def __init__(self, err):
        super().__init__(json.dumps(err))
        self.err = err
        self.text = str(err.get("message", "")) + " " + json.dumps(err.get("data", ""))


def rpc(method, params=None, timeout=60):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []}).encode()
    req = urllib.request.Request(RPC_URL, body, {"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=min(timeout, remaining())) as r:
            data = json.load(r)
    except urllib.error.HTTPError as e:
        data = json.loads(e.read() or b"{}")
    if data.get("error"):
        raise RpcError(data["error"])
    return data.get("result")


def try_rpc(method, params=None, timeout=60):
    """(result, error text): exactly one is not None; connection trouble counts as an error text."""
    try:
        return rpc(method, params, timeout), None
    except RpcError as e:
        return None, e.text
    except (OSError, ValueError) as e:
        return None, f"transport: {e}"


def tip():
    return rpc("getblockchaininfo")["blocks"]


def generate(n):
    rpc("generate", [n], timeout=120)


def mempool():
    return rpc("getrawmempool")


def funds():
    return rpc("wallet_spendable_funds")


def wait_for(what, fn, step=1.5, limit=120, mine=False):
    """Poll fn() until it returns something truthy; with mine, mine a block between polls."""
    end = min(time.time() + limit, DEADLINE)
    last = None
    while time.time() < end:
        try:
            last = fn()
        except (RpcError, OSError, ValueError, KeyError) as e:
            last = f"{type(e).__name__}: {e}"
        else:
            if last:
                return last
        if mine:
            generate(1)
        time.sleep(step)
    raise Abort(f"timed out waiting for {what} (last: {last})")


def render_config(ports, start):
    tpl = open(os.path.join(HERE, "node0.toml.in")).read()
    state = os.path.join(OUT, "state0")
    t = tpl.replace("@ROOT@/dilated_regtest/out/state0", state).replace("@ROOT@", HERE).replace("@START@", str(start))

    def sub(pattern, repl):
        nonlocal t
        t, n = re.subn(pattern, repl, t, flags=re.M)
        if n != 1:
            print(f"setup error: node0.toml.in changed, no unique match for {pattern!r}", file=sys.stderr)
            sys.exit(2)

    sub(r'^public_address = .*$', f'public_address = "127.0.0.1:{ports["public"]}"')
    sub(r'^listen_addr = "\[::\]:8233"$', f'listen_addr = "127.0.0.1:{ports["p2p"]}"')
    sub(r'^listen_addr = "127\.0\.0\.1:8232"$', f'listen_addr = "127.0.0.1:{ports["rpc"]}"')
    sub(r'^network_local_port = .*$', f'network_local_port = {ports["local"]}')
    sub(r'^network_initial_peers = .*$', 'network_initial_peers = []')
    # Exact confirmation counts matter here, so only `generate` mines (node1's setting in the
    # two-node run); at 90x dilation the internal miner would run past the staking window.
    sub(r'^internal_miner = true$', 'internal_miner = false')
    sub(r'^cache_dir = true$', 'cache_dir = false')  # no peer cache in the user's home
    return t


def stop_node():
    global proc
    p, proc = proc, None
    if p is None or p.poll() is not None:
        return
    try:
        os.killpg(p.pid, signal.SIGTERM)
    except OSError:
        pass
    try:
        p.wait(timeout=20)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except OSError:
            pass
        try:
            p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            pass


def cleanup(success=False, keep=False):
    stop_node()
    if success and OWN_OUT and not keep and OUT and os.path.isdir(OUT):
        shutil.rmtree(OUT, ignore_errors=True)


def node_log():
    s = ""
    for n in ("node0.log", "node0.err"):
        try:
            s += open(os.path.join(OUT, n), errors="replace").read()
        except OSError:
            pass
    return s


def stake_cmd(amount, fin):
    return {"CreateNewDelegationBond": {"amount_zats": amount, "target_finalizer": fin}}


def main():
    global proc, OUT, OWN_OUT, DEADLINE, RPC_URL
    ap = argparse.ArgumentParser(description="Headless regtest for issue #300 (wallet note merge before staking).")
    ap.add_argument("--zebrad", required=True, help="path to a built zebrad")
    ap.add_argument("--out", help="a NEW directory for the node's data and logs (must not exist)")
    ap.add_argument("--timeout", type=int, default=900, help="overall wall-clock limit in seconds (default 900)")
    ap.add_argument("--keep", action="store_true", help="keep the temporary output directory on success")
    a = ap.parse_args()

    zebrad = os.path.abspath(a.zebrad)
    if not (os.path.isfile(zebrad) and os.access(zebrad, os.X_OK)):
        print(f"setup error: no executable zebrad at {zebrad}", file=sys.stderr)
        return 2
    if a.out:
        OUT = os.path.abspath(a.out)
        if os.path.exists(OUT):
            print(f"setup error: --out {OUT} already exists; give a new directory", file=sys.stderr)
            return 2
        os.makedirs(OUT)
    else:
        OUT = tempfile.mkdtemp(prefix="wallet_notes_")
    OWN_OUT = not a.out
    DEADLINE = time.time() + a.timeout
    atexit.register(cleanup)
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda *_: sys.exit(2))

    ports = {k: free_port((0, 10000, 10001) if k == "p2p" else (0,))
             for k in ("rpc", "p2p", "public", "local")}
    RPC_URL = f"http://127.0.0.1:{ports['rpc']}"
    cfg = os.path.join(OUT, "node0.toml")
    open(cfg, "w").write(render_config(ports, int(time.time())))
    say(f"out={OUT} rpc={RPC_URL}")

    ok = False
    try:
        proc = subprocess.Popen([zebrad, "-c", cfg, "start"], cwd=OUT, stdin=subprocess.DEVNULL,
                                stdout=open(os.path.join(OUT, "node0.log"), "w"),
                                stderr=open(os.path.join(OUT, "node0.err"), "w"), start_new_session=True)
        run(zebrad)
        ok = not failures
    except Abort as e:
        say(f"FAIL: {e}")
        failures.append(str(e))
    except RpcError as e:
        say(f"FAIL: unexpected RPC error: {e}")
        failures.append(f"unexpected RPC error: {e}")
    except Exception as e:  # a bug in the test or an odd reply: fail, and keep the logs
        say(f"FAIL: unexpected {type(e).__name__}: {e}")
        failures.append(f"unexpected {type(e).__name__}: {e}")
    finally:
        if proc is not None and proc.poll() is not None:
            say(f"note: zebrad exited early with status {proc.returncode}")
        errs = [l for l in node_log().splitlines() if " ERROR " in l or "panicked" in l]
        if errs:
            say(f"node logged {len(errs)} ERROR/panic line(s); first: {errs[0][:200]}")
        cleanup(success=ok, keep=a.keep)
    if failures:
        say(f"FAILED ({len(failures)}): " + "; ".join(failures) + f"; logs in {OUT}")
        return 1
    say("PASS")
    return 0


def require(cond, msg):
    """A check later steps depend on: stop at once instead of waiting through them."""
    if not check(cond, msg):
        raise Abort(f"cannot continue: {msg}")


def find_keys(obj, needle):
    """Every (key, value) in a decoded JSON tree whose key contains needle, case-insensitively."""
    out = []
    if isinstance(obj, dict):
        for k, v in obj.items():
            if needle in k.lower():
                out.append((k, v))
            out += find_keys(v, needle)
    elif isinstance(obj, list):
        for v in obj:
            out += find_keys(v, needle)
    return out


def run(zebrad):
    # ---- startup: node up, wallet through its first sync pass, finalizer address from the log
    def up():
        if proc.poll() is not None:
            raise Abort(f"zebrad exited with status {proc.returncode}; see {OUT}/node0.err")
        return funds()
    f = wait_for("the wallet's first sync pass", up, step=2, limit=180)
    ua = f["address"]
    fin = wait_for("the finalizer address line",
                   lambda: (re.search(r"finalizer address: (zfinv1[A-Za-z0-9_-]+)", node_log()) or [None, None])[1])
    say(f"wallet {ua[:24]}... finalizer {fin[:24]}...")
    for k in ("spendable_zats", "pending_zats", "committed_zats", "max_send_zats", "max_stake_zats",
              "eligible_note_count", "max_notes_per_tx"):
        if not check(k in f, f"wallet_spendable_funds reports {k}"):
            raise Abort("node lacks the #300 wallet_spendable_funds fields; is zebrad built with the fix?")

    # ---- fund: enough mature coinbase for the faucet, then pay 0.5 cTAZ at a time
    if tip() < 40:
        generate(40 - tip())

    def donate(i):
        end = min(time.time() + 120, DEADLINE)
        while True:
            res, err = try_rpc("requestfaucetdonation", [{"address": ua}])
            if err is None:
                break
            if time.time() > end:
                raise Abort(f"faucet request {i} kept failing: {err}")
            time.sleep(2)  # "still pending" or the wallet is busy; the same address cannot queue twice
        if i == 1:
            check(res.get("amount") == FAUCET_ZATS,
                  f"the faucet pays the normal {FAUCET_ZATS} zats (0.5 cTAZ), got {res}")
        wait_for(f"faucet tx {i} in the mempool", lambda: len(mempool()) > 0, step=1, limit=150)
        # Mined, plus two blocks so the miner's change (3 confirmations, the tip counting as 1)
        # can fund the next faucet payment, and the wallet has seen all of it.
        generate(3)
        wait_for("the wallet to reach the tip", lambda: funds()["tip_height"] >= tip(), step=1, limit=90)

    def settle_notes(n):
        """Mine until the wallet has n mature faucet notes and nothing in flight."""
        def ready():
            f = funds()
            return f if (f["eligible_note_count"] == n and f["spendable_zats"] == n * FAUCET_ZATS
                         and f["pending_zats"] == 0 and f["committed_zats"] == 0) else None
        return wait_for(f"{n} spendable notes", ready, step=1.5, limit=150, mine=True)

    for i in range(1, 21):
        donate(i)
    f = settle_notes(20)
    say(f"20 notes settled at height {tip()}: {f}")
    check(f["max_notes_per_tx"] == 20, f"max_notes_per_tx is 20 (got {f['max_notes_per_tx']})")
    check(f["max_stake_zats"] < STAKE_ZATS, f"20 notes of 0.5 cTAZ cannot stake 10 cTAZ plus the fee "
          f"(max_stake_zats={f['max_stake_zats']})")

    h = tip()
    if h < STAKING_HEIGHT:
        generate(STAKING_HEIGHT - h)  # staking actions are illegal below the staking height
    wait_for("the wallet to see the tip", lambda: funds()["tip_height"] >= tip(), step=1)

    # ---- 1. exactly 10 cTAZ in 20 notes: no fee, so the stake must fail and must not broadcast
    _, err = try_rpc("wallet_staking_action", [stake_cmd(STAKE_ZATS, fin)])
    check(err is not None, "a 10 cTAZ stake from exactly 10 cTAZ (no fee) is refused")
    say(f"      error was: {err}")
    check(err is not None and re.search(r"insufficient|not enough|fee|funds", err, re.I) is not None,
          "the refusal says the funds are insufficient (fee)")
    time.sleep(3)
    check(mempool() == [], "the refused stake left nothing in the mempool")

    # ---- 2. invalid bond amount, missing bond
    _, err = try_rpc("wallet_staking_action", [stake_cmd(12_345_678, fin)])
    check(err is not None and re.search(r"power|bond|amount|invalid", err, re.I) is not None,
          f"a bond of 12345678 zats (not a power of 10) is refused with a bond/amount error ({err})")
    missing = "00" * 31 + "01"
    _, err = try_rpc("wallet_staking_action", [{"BeginDelegationUnbonding": {"bond_key": missing}}])
    if err is not None and re.search(r"-32602|invalid (type|value|params|length)|unknown variant", err, re.I):
        say(f"skip: could not form a bond_key request this build accepts ({err})")
    else:
        check(err is not None and re.search(r"bond", err, re.I) is not None,
              f"unbonding a bond the wallet does not have is refused with a missing-bond error ({err})")
    check(mempool() == [], "the invalid requests left nothing in the mempool")

    # ---- 3. 21 notes: over the cap
    donate(21)
    f = settle_notes(21)
    say(f"21 notes settled at height {tip()}: {f}")
    check(f["spendable_zats"] == 21 * FAUCET_ZATS, "21 notes, 10.5 cTAZ spendable in total")
    check(f["max_stake_zats"] < STAKE_ZATS,
          f"only {f['max_notes_per_tx']} notes fit one transaction, so max_stake_zats "
          f"({f['max_stake_zats']}) stays below 10 cTAZ although 10.5 cTAZ is spendable")
    check(f["max_send_zats"] < f["spendable_zats"], "max_send_zats is below spendable_zats (input cap)")
    _, err = try_rpc("wallet_staking_action", [stake_cmd(STAKE_ZATS, fin)])
    require(err is not None, "the 10 cTAZ stake from 21 notes fails before any merge")
    say(f"      error was: {err}")
    check(err is not None and re.search(r"\b20\b", err) and re.search(r"note|input", err, re.I)
          and re.search(r"merge|consolidat|combine", err, re.I),
          "the error names the 20-note input cap and says to merge the notes")
    time.sleep(3)
    check(mempool() == [], "no automatic merge or retry was broadcast")

    # ---- 4. explicit merge, mined, confirmed, then the stake
    txid, err = try_rpc("wallet_merge_notes", [])
    require(err is None and isinstance(txid, str) and re.fullmatch(r"[0-9a-fA-F]{64}", txid) is not None,
            f"wallet_merge_notes returns a txid ({txid or err})")
    before_total = 21 * FAUCET_ZATS
    wait_for("the merge in the mempool", lambda: txid in mempool(), step=1, limit=150)
    check(mempool() == [txid], "the merge is the only transaction in the mempool")
    generate(1)
    tx = rpc("getrawtransaction", [txid, 1])
    check(isinstance(tx, dict) and tx.get("confirmations") == 1 and tx.get("height", 0) > 0,
          f"the merge is mined and confirmed once (confirmations={tx.get('confirmations') if isinstance(tx, dict) else tx})")
    wait_for("the wallet to see the merge mined",
             lambda: funds()["pending_zats"] > 0 and funds()["committed_zats"] == 0, step=1, limit=60)
    f = funds()
    say(f"after the merge, 1 conf: {f}")
    # Fee conservation: the merge moves value into one self note and burns only the fee.
    total = f["spendable_zats"] + f["pending_zats"] + f["committed_zats"]
    fee = before_total - total
    check(f["committed_zats"] == 0 and 0 < fee <= ZAT // 100,
          f"the merge conserved value up to a fee: wallet total {total} of {before_total}, fee {fee} zats")
    bal = [v for k, v in find_keys(tx, "valuebalance") if isinstance(v, int) and v != 0]
    if bal:
        check(all(abs(v) == fee for v in bal), f"the merge transaction's shielded value balance {bal} equals the wallet's fee {fee}")
    else:
        say("note: getrawtransaction shows no shielded value balance field; the fee was checked from wallet totals only")
    check(f["max_stake_zats"] < STAKE_ZATS, "the merged note is immature, so 10 cTAZ is not stakeable yet")
    _, err = try_rpc("wallet_staking_action", [stake_cmd(STAKE_ZATS, fin)])
    require(err is not None, "a stake while the merge output has 1 confirmation is refused")
    say(f"      error was: {err}")
    check(err is not None and re.search(r"confirm|wait|pending|mature|immature", err, re.I) is not None,
          "the refusal gives a wait reason (confirmations / pending change)")
    time.sleep(3)
    check(mempool() == [], "the refused stake was not retried in the background")

    generate(2)  # 3 confirmations, the tip block counting as 1
    f = wait_for("the merged note to be spendable",
                 lambda: (lambda f: f if f["max_stake_zats"] >= STAKE_ZATS and f["pending_zats"] == 0 else None)(funds()),
                 step=1.5, limit=90)
    say(f"after the merge, 3 confs: {f}")
    check(f["eligible_note_count"] <= 2, f"21 notes are down to {f['eligible_note_count']} (the merged one and the spare)")
    res, err = try_rpc("wallet_staking_action", [stake_cmd(STAKE_ZATS, fin)])
    check(err is None, f"the 10 cTAZ stake succeeds after the merge ({res or err})")
    wait_for("the stake in the mempool", lambda: len(mempool()) > 0, step=1, limit=150)
    generate(1)
    h = tip()
    check(h < ROSTER_HEIGHT, f"the bond landed at height {h}, before the roster height {ROSTER_HEIGHT}")

    def positioned():
        pos = rpc("wallet_staking_positions")
        return pos if any(len(v) > 0 for v in pos.get("active", {}).values()) else None
    pos = wait_for("the bond in wallet_staking_positions", positioned, step=1.5, limit=90, mine=False)
    vals = [p.get("latest_val") for v in pos["active"].values() for p in v]
    check(STAKE_ZATS in vals, f"an active bond of {STAKE_ZATS} zats exists (latest_val {vals})")


if __name__ == "__main__":
    sys.exit(main())
