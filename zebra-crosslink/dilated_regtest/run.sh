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
tip() { rpc "$1" getblockchaininfo | jq -r '.result.blocks // 0'; }
logs() { cat "$OUT/node$1.log" "$OUT/node$1.err" 2>/dev/null | tr -d '\r'; }
log() { echo "$(date +%T) tip=$(tip 0) $*"; }
fail() { echo "FAIL: $*"; FAIL=1; }
sample() {
  for n in 0 1; do
    L=$(logs $n)
    echo "$(date +%T) node$n tip=$(tip $n) fin=$(rpc $n get_tfl_final_block_height_and_hash | jq -r '.result.height // "-"') decided=$(echo "$L" | grep -c 'Successfully crosslink-finalized') payouts=$(echo "$L" | grep -c 'PoS payout decision.*payout=true') advanced=$(echo "$L" | grep -c 'cert_advanced=true') errors=$(echo "$L" | grep -c ' ERROR ')"
  done
}
stop_nodes() {
  for p in "${PID[@]}"; do
    if [ $WIN = 1 ]; then taskkill //PID "$p" //F >/dev/null 2>&1; else kill "$p" 2>/dev/null; fi
  done
}
trap stop_nodes EXIT

mkdir -p "$OUT"; rm -rf "$OUT/state0" "$OUT/state1"; : > "$OUT/node0.log"; : > "$OUT/node1.log"
START=$(date +%s)
for n in 0 1; do
  sed "s|@ROOT@|$ROOT_TOML|g; s|@START@|$START|g" "$HERE/node$n.toml.in" > "$OUT/node$n.toml"
  if [ $WIN = 1 ]; then
    PID[$n]=$(powershell -NoProfile -Command "(Start-Process -FilePath '$(cygpath -w "$ZEBRAD")' -ArgumentList '-c','$(cygpath -w "$OUT/node$n.toml")','start' -WorkingDirectory '$(cygpath -w "$ROOT")' -RedirectStandardOutput '$(cygpath -w "$OUT/node$n.log")' -RedirectStandardError '$(cygpath -w "$OUT/node$n.err")' -PassThru).Id")
  else
    (cd "$ROOT" && "$ZEBRAD" -c "$OUT/node$n.toml" start > "$OUT/node$n.log" 2> "$OUT/node$n.err") & PID[$n]=$!
  fi
  echo "node$n pid=${PID[$n]}"
done

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

#-- mine flat out to TARGET
n=0
while [ "$(tip 0)" -lt "$TARGET" ]; do
  rpc 0 generate '[1]' >/dev/null; n=$((n + 1)); [ $((n % 50)) = 0 ] && sample
done
sleep 5; sample

#-- verdict
T0=$(tip 0); T1=$(tip 1)
[ "$T0" -ge "$TARGET" ] || fail "node0 tip $T0 < $TARGET"
[ $((T0 - T1)) -le 2 ] && [ $((T1 - T0)) -le 2 ] || fail "nodes out of sync: $T0 vs $T1"
logs 0 | grep -q "crosslink bootstrap: PoW reached height $ACTIVATION_HEIGHT" || fail "no BFT bootstrap on node0"
for k in 0 1; do
  L=$(logs $k)
  echo "$L" | grep -q "panicked" && fail "node$k panicked"
  echo "$L" | grep -q "empty roster" && fail "node$k has an empty bootstrap roster"
  bad=$(echo "$L" | grep ' ERROR ' | grep -vc 'not yet implemented: all the documented validations')
  [ "$bad" = 0 ] || fail "node$k logged $bad unexpected ERROR lines"
  dec=$(echo "$L" | grep -c 'Successfully crosslink-finalized'); need=$(( (TARGET - ACTIVATION_HEIGHT) / 4 ))
  [ "$dec" -ge "$need" ] || fail "node$k decided $dec BFT blocks, expected at least $need"
  fin=$(rpc $k get_tfl_final_block_height_and_hash | jq -r '.result.height // 0')
  [ $((T0 - fin)) -le 40 ] || fail "node$k final height $fin lags tip $T0 by more than 40"
done
if [ $FAIL = 0 ]; then echo "PASS: dilated regtest to height $T0 in $(( $(date +%s) - START )) s"; else echo "FAILED after $(( $(date +%s) - START )) s; logs in $OUT"; fi
exit $FAIL
