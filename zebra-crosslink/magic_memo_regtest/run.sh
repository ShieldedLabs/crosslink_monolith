#!/bin/bash
# Experimental magic memo system test on a fresh single-node regtest: funds the user wallet, sends a
# multi-chunk magic memo with the wallet_send_magic_memo RPC, mines it, and checks that the wallet's
# compact-block scanner reassembles exactly the text that was sent, reported exactly once.
#
#   zebra-crosslink/magic_memo_regtest/run.sh [encrypted|public|both]    (default: both)
#
# encrypted: the memo is encrypted to the miner wallet's address, and only the miner wallet may report it.
# public:    the memo is plaintext, reported once however many wallets scan it.
#
# Needs curl, jq and a debug zebrad built with the feature, from zebra-crosslink/:
#   cargo build -p zebrad --features magic-memo        (or set ZEBRAD=<path to zebrad>)
# Runs from Git Bash on Windows or a Linux shell. Everything it writes goes to out/ (ignored by git):
# the rendered config, the node state and the node log of the last mode run. Exit status 0 is a pass.
# Uses ports 18432 (RPC), 18433 (p2p), 28434 (light wallet), 12101 and 12401, so it can run beside
# other local nodes. A debug build takes about five minutes per mode, most of it funding the wallet.
set -u
MODE=${1:-both}
HERE=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$HERE/.." && pwd); OUT=$HERE/out
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) WIN=1; OUT_TOML=$(mkdir -p "$OUT" && cd "$OUT" && pwd -W);; *) WIN=0; OUT_TOML=$OUT;; esac
ZEBRAD=${ZEBRAD:-$ROOT/target/debug/zebrad}; [ $WIN = 1 ] && [ "${ZEBRAD%.exe}" = "$ZEBRAD" ] && ZEBRAD=$ZEBRAD.exe
[ -x "$ZEBRAD" ] || { echo "no zebrad at $ZEBRAD"; exit 2; }
PORT=18432
# The miner wallet's seed is fixed, so its address is too.
MINER_UA=utest14wa0pcf7uusm364sz8ewd0kg5x7fud4nmph6nm55f300l658nmaa0tstc6hssfnn44gw90utujn4wsrl7u6kuvel6yya8muzgcz6tyz9
MEMO='Hello from a Crosslink magic memo! This payload is longer than one action can carry, so it is split into chunks across several zero-value Ironwood actions, each read straight from compact blocks.'

rpc() { curl -s -m "${3:-30}" -X POST "http://127.0.0.1:$PORT" -H 'content-type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":${2:-[]}}"; }
tip() { t=$(rpc getblockchaininfo | jq -r '.result.blocks // 0' 2>/dev/null); case "$t" in ''|*[!0-9]*) t=0;; esac; echo "$t"; }
logs() { cat "$OUT/node.log" "$OUT/node.err" 2>/dev/null | tr -d '\r'; }
log() { echo "$(date +%T) tip=$(tip) $*"; }
PID=
stop_node() {
  [ -n "$PID" ] || return
  if [ $WIN = 1 ]; then taskkill //PID "$PID" //F >/dev/null 2>&1; else kill "$PID" 2>/dev/null; fi
  PID=
}
trap stop_node EXIT

# Runs one mode; returns 0 on a pass.
trial() {
  local mode=$1 expect params D n f s p r memo_txid seen got
  if [ "$mode" = encrypted ]; then expect="decrypted by miner wallet"; params=$(jq -cn --arg m "$MEMO" --arg to "$MINER_UA" '[$m, $to]')
  else expect=public; params=$(jq -cn --arg m "$MEMO" '[$m]'); fi
  echo "===== $mode: a ${#MEMO}-byte memo, expecting the scanner to report it as: $expect"

  stop_node
  rm -rf "$OUT/state"; rm -f "$OUT"/node.log "$OUT"/node.err "$OUT"/node.pid
  sed "s|@OUT@|$OUT_TOML|g" "$HERE/node.toml.in" > "$OUT/node.toml"
  if [ $WIN = 1 ]; then
    # The pid goes through a file, as in dilated_regtest/run.sh: a `$(...)` capture of PowerShell's output would
    # never return while the node holds the inherited pipe open.
    powershell -NoProfile -Command "(Start-Process -FilePath '$(cygpath -w "$ZEBRAD")' -ArgumentList '-c','$(cygpath -w "$OUT/node.toml")','start','--headless' -WorkingDirectory '$(cygpath -w "$OUT")' -RedirectStandardOutput '$(cygpath -w "$OUT/node.log")' -RedirectStandardError '$(cygpath -w "$OUT/node.err")' -PassThru).Id | Set-Content -Encoding ascii '$(cygpath -w "$OUT/node.pid")'" >/dev/null 2>&1 </dev/null
    PID=$(tr -dc '0-9' < "$OUT/node.pid")
  else
    (cd "$OUT" && "$ZEBRAD" -c "$OUT/node.toml" start --headless > "$OUT/node.log" 2> "$OUT/node.err") & PID=$!
  fi
  [ -n "$PID" ] || { echo "node did not start; see $OUT/node.err"; return 1; }
  log "node pid=$PID"

  D=$(( $(date +%s) + 600 ))
  until ua=$(rpc wallet_spendable_funds | jq -r '.result.address // empty' 2>/dev/null) && [ -n "$ua" ]; do
    [ "$(date +%s)" -lt $D ] || { log "FAIL: the wallet never reported an address"; return 1; }; sleep 3
  done
  log "user wallet $ua"
  r=$(rpc wallet_send_magic_memo "$params" 300 | jq -r '.error.message // empty')
  [ -n "$r" ] || { log "FAIL: the RPC should refuse while the wallet has no funds"; return 1; }
  log "unfunded, the RPC refuses: $r"

  # The faucet pays from the miner's shielded funds, which only exist after its first auto-shield, and a
  # request it cannot pay is dropped; so ask again every few blocks until the user wallet sees the payment.
  D=$(( $(date +%s) + 1200 )); n=0
  until f=$(rpc wallet_spendable_funds | jq -c '.result') && s=$(echo "$f" | jq -r '.spendable_zats // 0') && [ "$s" -ge 1000000 ]; do
    [ "$(date +%s)" -lt $D ] || { log "FAIL: the user wallet never became spendable (last: $f)"; return 1; }
    p=$(echo "$f" | jq -r '(.pending_zats // 0) + (.committed_zats // 0)')
    if [ "$p" = 0 ] && [ $((n % 5)) = 0 ]; then rpc requestfaucetdonation "[{\"address\":\"$ua\"}]" >/dev/null; fi
    n=$((n + 1)); rpc generate '[1]' >/dev/null; sleep 3
  done
  log "user spendable: $s zats"

  # The RPC answers once the transaction is built and broadcast, and proving is slow in a debug build. A
  # broadcast that times out in mempool verification (also slow in debug) comes back as an error; retry.
  D=$(( $(date +%s) + 900 ))
  until r=$(rpc wallet_send_magic_memo "$params" 300) && memo_txid=$(echo "$r" | jq -r '.result // empty' | grep -o '[0-9a-f]\{64\}') && [ -n "$memo_txid" ]; do
    [ "$(date +%s)" -lt $D ] || { log "FAIL: the RPC never published the memo (last: $r)"; return 1; }
    log "RPC: $(echo "$r" | jq -c '.result // .error.message'); retrying"
    rpc generate '[1]' >/dev/null; sleep 5
  done
  log "the RPC published the memo in $memo_txid"
  until logs | grep -a -q "MAGIC MEMO at"; do
    [ "$(date +%s)" -lt $D ] || { log "FAIL: the scanner never reported the memo"; return 1; }
    sleep 2; rpc generate '[1]' >/dev/null
  done

  seen=$(logs | grep -a "MAGIC MEMO at" | head -1)
  log "scanner: $seen"
  got=$(echo "$seen" | sed -n 's/.*bytes): "\(.*\)"$/\1/p')
  [ "$(logs | grep -a -c "MAGIC MEMO at")" = 1 ] || { log "FAIL: expected exactly one scanner report"; return 1; }
  case "$seen" in *"$memo_txid ($expect,"*) ;; *) log "FAIL: expected the scanner to report '$expect' in $memo_txid"; return 1;; esac
  [ "$got" = "$MEMO" ] || { log "FAIL: the reassembled memo differs"; echo "want: $MEMO"; echo "got:  $got"; return 1; }
  log "PASS ($mode)"
}

mkdir -p "$OUT"
case "$MODE" in
  encrypted|public) trial "$MODE"; RC=$? ;;
  both) trial encrypted; RC=$?; [ $RC = 0 ] && cp "$OUT/node.log" "$OUT/node_encrypted.log" && { trial public; RC=$?; } ;;
  *) echo "usage: $0 [encrypted|public|both]"; exit 2 ;;
esac
[ $RC = 0 ] && echo "PASSED" || echo "FAILED; the node log is $OUT/node.log"
exit $RC
