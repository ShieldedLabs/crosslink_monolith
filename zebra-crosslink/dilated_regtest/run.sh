#!/bin/bash
# Time-dilated two-node regtest: funds node 0's wallet, bonds stake to both finalizers before the
# bootstrap roster height, mines flat out to TARGET, and checks that BFT bootstrapped, decides
# blocks and keeps the final height near the tip. Needs a built zebrad (phuild.bat zebra-crosslink
# Debug Win64 -p zebrad), curl and jq. See DILATED_REGTEST.md at the repository root.
#
#   dilated_regtest/run.sh [TARGET=450]
#
# Mining never pauses once stake is placed: an idle tip makes every BFT proposal empty and the
# round timeouts grow with the round number, so a pause of minutes costs minutes more to recover.
set -u
TARGET=${1:-450}
HERE=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$HERE/.." && pwd); OUT=$HERE/out
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) WIN=1; ROOT_TOML=$(cd "$ROOT" && pwd -W);; *) WIN=0; ROOT_TOML=$ROOT;; esac
ZEBRAD=$ROOT/target/debug/zebrad; [ $WIN = 1 ] && ZEBRAD=$ZEBRAD.exe
[ -x "$ZEBRAD" ] || { echo "no zebrad at $ZEBRAD"; exit 2; }
BOND=20000000; ROSTER_HEIGHT=75; ACTIVATION_HEIGHT=275
PORT=(8232 8242); PID=(); FAIL=0

rpc() { curl -s -m 300 -X POST "http://127.0.0.1:${PORT[$1]}" -H 'content-type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":${3:-[]}}"; }
# Never empty: an RPC that times out or answers unparseably reports 0, so a `-lt` against it
# stays a comparison instead of erroring out and ending the loop that made it.
tip() { t=$(rpc "$1" getblockchaininfo | jq -r '.result.blocks // 0' 2>/dev/null); case "$t" in ''|*[!0-9]*) t=0;; esac; echo "$t"; }
logs() { cat "$OUT"/node$1.log* "$OUT"/node$1.err* 2>/dev/null | tr -d '\r'; }
log() { echo "$(date +%T) tip=$(tip 0) $*"; }
fail() { echo "FAIL: $*"; FAIL=1; }
sample() {
  for n in 0 1; do
    L=$(logs $n)
    echo "$(date +%T) node$n tip=$(tip $n) fin=$(rpc $n get_tfl_final_block_height_and_hash | jq -r '.result.height // "-"') decided=$(echo "$L" | grep -c 'Successfully decided BFT block') payouts=$(echo "$L" | grep -c 'PoS payout decision.*payout=true') advanced=$(echo "$L" | grep -c 'cert_advanced=true') errors=$(echo "$L" | grep -c ' ERROR ')"
  done
}
stop_nodes() {
  for p in "${PID[@]}"; do
    if [ $WIN = 1 ]; then taskkill //PID "$p" //F >/dev/null 2>&1; else kill "$p" 2>/dev/null; fi
  done
}
trap stop_nodes EXIT

start_node() {
  n=$1
  sed "s|@ROOT@|$ROOT_TOML|g; s|@START@|$START|g" "$HERE/node$n.toml.in" > "$OUT/node$n.toml"
  if [ $WIN = 1 ]; then
    # The pid goes through a file rather than a `$(...)` capture: the node inherits PowerShell's
    # standard output, so when that is the capture's pipe the node holds it open for the whole
    # run and the capture never returns -- node 1 would never start. Sending both of PowerShell's
    # streams to the void leaves the node nothing worth inheriting.
    powershell -NoProfile -Command "(Start-Process -FilePath '$(cygpath -w "$ZEBRAD")' -ArgumentList '-c','$(cygpath -w "$OUT/node$n.toml")','start' -WorkingDirectory '$(cygpath -w "$ROOT")' -RedirectStandardOutput '$(cygpath -w "$OUT/node$n.log")' -RedirectStandardError '$(cygpath -w "$OUT/node$n.err")' -PassThru).Id | Set-Content -Encoding ascii '$(cygpath -w "$OUT/node$n.pid")'" >/dev/null 2>&1 </dev/null
    PID[$n]=$(tr -dc '0-9' < "$OUT/node$n.pid")
    [ -n "${PID[$n]}" ] || { echo "node$n did not start; see $OUT/node$n.err"; exit 2; }
  else
    (cd "$ROOT" && "$ZEBRAD" -c "$OUT/node$n.toml" start > "$OUT/node$n.log" 2> "$OUT/node$n.err") & PID[$n]=$!
  fi
  echo "node$n pid=${PID[$n]}"
}

mkdir -p "$OUT"; rm -rf "$OUT/state0" "$OUT/state1"; rm -f "$OUT"/node?.log* "$OUT"/node?.err*
START=$(date +%s)
for n in 0 1; do start_node $n; done

# The finalizer addresses come from each node's own startup line.
for n in 0 1; do
  until FIN[$n]=$(logs $n | grep -o 'finalizer address: zfinv1[A-Za-z0-9_-]*' | head -1 | cut -d' ' -f3) && [ -n "${FIN[$n]}" ]; do sleep 2; done
  echo "node$n finalizer ${FIN[$n]:0:24}..."
done

#-- fund and stake, mining only as much as each step needs
until ua=$(rpc 0 wallet_spendable_funds | jq -r '.result.address // empty') && [ -n "$ua" ]; do log "waiting for the wallet"; sleep 3; done
[ "$(tip 0)" -lt 4 ] && rpc 0 generate '[4]' >/dev/null
log "faucet: $(rpc 0 requestfaucetdonation "[{\"address\":\"$ua\"}]" | jq -c '.result // .error')"
settle() { # wait for the wallet's tx to reach the mempool, mine it in, mine until $1 zats are spendable
  until [ "$(rpc 0 getrawmempool | jq -r '.result|length')" != 0 ]; do sleep 2; done
  rpc 0 generate '[2]' >/dev/null
  until s=$(rpc 0 wallet_spendable_funds | jq -r '.result.spendable_zats // empty') && [ -n "$s" ] && [ "$s" -ge "$1" ]; do rpc 0 generate '[1]' >/dev/null; sleep 2; done
}
settle $((2 * BOND + 1000000))
for n in 0 1; do
  cmd=$(jq -cn --arg f "${FIN[$n]}" --argjson a "$BOND" '{CreateNewDelegationBond:{amount_zats:$a,target_finalizer:$f}}')
  log "bond -> node$n: $(rpc 0 staking_command "$(jq -cn --arg c "$cmd" '[$c]')" | jq -c 'if .error then .error else "submitted" end')"
  if [ $n = 0 ]; then settle $((BOND + 1000000)); else settle 0; fi
done
rpc 0 generate '[1]' >/dev/null
staked=$(tip 0); log "positions: $(rpc 0 wallet_staking_positions | jq -c '.result.active | map_values(length)')"
[ "$staked" -lt "$ROSTER_HEIGHT" ] || fail "bonds landed at height $staked, at or past the roster height $ROSTER_HEIGHT"

#-- mine to the restart point, restart both nodes against the same state, then mine on to TARGET
restart_nodes() {
  for k in 0 1; do DEC_BEFORE[$k]=$(logs $k | grep -c "Successfully decided BFT block"); done
  BEFORE=$(tip 0); echo "$(date +%T) restart: stopping both nodes at tip $BEFORE"
  stop_nodes
  for p in "${PID[@]}"; do
    if [ $WIN = 1 ]; then
      while [ "$(powershell -NoProfile -Command "(Get-Process -Id $p -ErrorAction SilentlyContinue | Measure-Object).Count" 2>/dev/null | tr -dc 0-9)" != 0 ]; do sleep 1; done
    else while kill -0 "$p" 2>/dev/null; do sleep 1; done; fi
  done
  # The new process truncates node$n.log, so the pre-restart logs move aside; `logs` globs both,
  # while the bare .log is what the post-restart checks read.
  for k in 0 1; do mv "$OUT/node$k.log" "$OUT/node$k.log.1"; mv "$OUT/node$k.err" "$OUT/node$k.err.1"; done
  PID=()
  for k in 0 1; do start_node $k; done
  # The tip is not expected back at $BEFORE here: a hard kill drops whatever the non-finalized
  # state held above the committed height, and node 0 re-mines it in `mine_to`.
  for k in 0 1; do until [ "$(tip $k)" -gt 0 ]; do sleep 2; done; done
  echo "$(date +%T) restart: both nodes back at tip $(tip 0)"
}

mine_to() {
  n=0
  while [ "$(tip 0)" -lt "$1" ]; do
    rpc 0 generate "[1]" >/dev/null; n=$((n + 1)); [ $((n % 50)) = 0 ] && sample
  done
}
RESTART_HEIGHT=$(( (ACTIVATION_HEIGHT + TARGET) / 2 ))
mine_to "$RESTART_HEIGHT"
# A restart below the activation height would test nothing, so stop rather than report four
# confusing failures about a BFT chain that was never built.
[ "$(tip 0)" -gt "$ACTIVATION_HEIGHT" ] || { fail "mining stopped at $(tip 0), below the activation height $ACTIVATION_HEIGHT"; exit 1; }
sleep 5
restart_nodes
mine_to "$TARGET"
sleep 5; sample

#-- verdict
T0=$(tip 0); T1=$(tip 1)
[ "$T0" -ge "$TARGET" ] || fail "node0 tip $T0 < $TARGET"
[ $((T0 - T1)) -le 2 ] && [ $((T1 - T0)) -le 2 ] || fail "nodes out of sync: $T0 vs $T1"
logs 0 | grep -q "crosslink bootstrap: PoW reached height $ACTIVATION_HEIGHT" || fail "no BFT bootstrap on node0"
# Post-restart only: the bare .log/.err pair belongs to the second process.
for k in 0 1; do
  P=$(cat "$OUT/node$k.log" "$OUT/node$k.err" 2>/dev/null | tr -d "\r")
  h=$(echo "$P" | grep -o "crosslink restore: resuming BFT at height [0-9]*" | tail -1 | grep -o "[0-9]*$")
  [ -n "$h" ] || fail "node$k did not resume BFT from its database after the restart"
  echo "$P" | grep -q "crosslink bootstrap: PoW reached height" && fail "node$k re-bootstrapped BFT after the restart"
  # One decision may have committed without its row being written, so the resumed height is
  # allowed to be one short of what the pre-restart log counted.
  [ -z "$h" ] || [ "$h" -ge $(( ${DEC_BEFORE[$k]} - 1 )) ] || fail "node$k resumed BFT at height $h, behind the ${DEC_BEFORE[$k]} blocks it had decided"
  after=$(echo "$P" | grep -c "Successfully decided BFT block")
  [ "$after" -ge 1 ] || fail "node$k decided no BFT blocks after the restart"
done
[ -z "$(find "$OUT" -name pos.chain 2>/dev/null)" ] || fail "a PoS store file still exists under $OUT"
for k in 0 1; do
  L=$(logs $k)
  echo "$L" | grep -q "panicked" && fail "node$k panicked"
  echo "$L" | grep -q "empty roster" && fail "node$k has an empty bootstrap roster"
  bad=$(echo "$L" | grep ' ERROR ' | grep -vc 'not yet implemented: all the documented validations')
  [ "$bad" = 0 ] || fail "node$k logged $bad unexpected ERROR lines"
  dec=$(echo "$L" | grep -c 'Successfully decided BFT block'); need=$(( (TARGET - ACTIVATION_HEIGHT) / 4 ))
  [ "$dec" -ge "$need" ] || fail "node$k decided $dec BFT blocks, expected at least $need"
  fin=$(rpc $k get_tfl_final_block_height_and_hash | jq -r '.result.height // 0')
  [ $((T0 - fin)) -le 40 ] || fail "node$k final height $fin lags tip $T0 by more than 40"
done
if [ $FAIL = 0 ]; then echo "PASS: dilated regtest to height $T0 in $(( $(date +%s) - START )) s"; else echo "FAILED after $(( $(date +%s) - START )) s; logs in $OUT"; fi
exit $FAIL
