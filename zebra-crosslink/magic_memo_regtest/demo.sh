#!/bin/bash
# Experimental: a fresh single-node regtest with the GUI, for demoing memos. Funds the user wallet from the faucet,
# then mines a block every few seconds so that sends confirm while you watch. Close the GUI window (or press
# Ctrl+C here) to stop.
#
#   zebra-crosslink/magic_memo_regtest/demo.sh            (BLOCK_SECS=10 by default; CLIP=0 leaves the clipboard alone)
#
# In the GUI: Send > Paste Address (the miner address printed below, also put on the clipboard), type a memo,
# pick Normal / Magic public / Magic encrypted, optionally pad, then press an amount or "Memo only". The memo shows
# on the transaction's row in the user and miner tabs; the row's layout button shows how its bytes sit in the
# transaction's actions. User > "Decrypt magic memos sent to me" switches decryption off and on.
#
# Same build and ports as run.sh: a debug zebrad built from zebra-crosslink/ with
#   cargo build -p zebrad --features magic-memo        (or set ZEBRAD=<path to zebrad>)
# State and logs go to out/demo/ (ignored by git) and start fresh each run.
set -u
HERE=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$HERE/.." && pwd); OUT=$HERE/out/demo
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) WIN=1; OUT_TOML=$(mkdir -p "$OUT" && cd "$OUT" && pwd -W);; *) WIN=0; mkdir -p "$OUT"; OUT_TOML=$OUT;; esac
ZEBRAD=${ZEBRAD:-$ROOT/target/debug/zebrad}; [ $WIN = 1 ] && [ "${ZEBRAD%.exe}" = "$ZEBRAD" ] && ZEBRAD=$ZEBRAD.exe
[ -x "$ZEBRAD" ] || { echo "no zebrad at $ZEBRAD"; exit 2; }
BLOCK_SECS=${BLOCK_SECS:-10}
PORT=18432
# The miner wallet's seed is fixed, so its address is too.
MINER_UA=utest14wa0pcf7uusm364sz8ewd0kg5x7fud4nmph6nm55f300l658nmaa0tstc6hssfnn44gw90utujn4wsrl7u6kuvel6yya8muzgcz6tyz9

rpc() { curl -s -m "${3:-30}" -X POST "http://127.0.0.1:$PORT" -H 'content-type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":${2:-[]}}"; }
tip() { t=$(rpc getblockchaininfo | jq -r '.result.blocks // 0' 2>/dev/null); case "$t" in ''|*[!0-9]*) t=0;; esac; echo "$t"; }
log() { echo "$(date +%T) tip=$(tip) $*"; }
alive() { if [ $WIN = 1 ]; then tasklist //FI "PID eq $PID" 2>/dev/null | grep -q "$PID"; else kill -0 "$PID" 2>/dev/null; fi; }
PID=
stop_node() {
  [ -n "$PID" ] || return
  if [ $WIN = 1 ]; then taskkill //PID "$PID" //F >/dev/null 2>&1; else kill "$PID" 2>/dev/null; fi
  PID=
}
trap stop_node EXIT

rm -rf "$OUT/state"; rm -f "$OUT"/node.log "$OUT"/node.err "$OUT"/node.pid
sed "s|@OUT@|$OUT_TOML|g" "$HERE/node.toml.in" > "$OUT/node.toml"
if [ $WIN = 1 ]; then
  powershell -NoProfile -Command "(Start-Process -FilePath '$(cygpath -w "$ZEBRAD")' -ArgumentList '-c','$(cygpath -w "$OUT/node.toml")','start' -WorkingDirectory '$(cygpath -w "$OUT")' -RedirectStandardOutput '$(cygpath -w "$OUT/node.log")' -RedirectStandardError '$(cygpath -w "$OUT/node.err")' -PassThru).Id | Set-Content -Encoding ascii '$(cygpath -w "$OUT/node.pid")'" >/dev/null 2>&1 </dev/null
  PID=$(tr -dc '0-9' < "$OUT/node.pid")
else
  (cd "$OUT" && "$ZEBRAD" -c "$OUT/node.toml" start > "$OUT/node.log" 2> "$OUT/node.err") & PID=$!
fi
[ -n "$PID" ] || { echo "node did not start; see $OUT/node.err"; exit 1; }
log "node pid=$PID; logs in $OUT"

until ua=$(rpc wallet_spendable_funds | jq -r '.result.address // empty' 2>/dev/null) && [ -n "$ua" ]; do
  alive || { echo "the node exited; see $OUT/node.err"; exit 1; }; sleep 3
done
echo
echo "Miner address, the recipient for memos$([ "${CLIP:-1}" = 1 ] && echo ' (also on the clipboard)'):"
echo "  $MINER_UA"
echo "User address: $ua"
echo
if [ "${CLIP:-1}" = 1 ]; then
  if [ $WIN = 1 ]; then printf '%s' "$MINER_UA" | clip.exe; elif command -v xclip >/dev/null; then printf '%s' "$MINER_UA" | xclip -selection clipboard; fi
fi

# The faucet pays from the miner's shielded funds, which only exist after its first auto-shield, and a request it
# cannot pay is dropped; so ask again every few blocks until the user wallet sees the payment.
n=0
until s=$(rpc wallet_spendable_funds | jq -r '.result.spendable_zats // 0') && [ "$s" -ge 1000000 ]; do
  alive || exit 0
  p=$(rpc wallet_spendable_funds | jq -r '(.result.pending_zats // 0) + (.result.committed_zats // 0)')
  if [ "$p" = 0 ] && [ $((n % 5)) = 0 ]; then rpc requestfaucetdonation "[{\"address\":\"$ua\"}]" >/dev/null; fi
  n=$((n + 1)); rpc generate '[1]' >/dev/null; sleep 3
done
log "user wallet funded with $s zats; mining a block every ${BLOCK_SECS}s"

while alive; do
  rpc generate '[1]' >/dev/null; sleep "$BLOCK_SECS"
done
echo "the node exited"
