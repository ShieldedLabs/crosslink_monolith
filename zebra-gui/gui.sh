#!/usr/bin/env bash
# The standalone GUI's build, scripted run, checks, benchmark and tests. GUI_DEV.md is the manual.
#
#   bash zebra-gui/gui.sh snap base                  Before editing: record every screen as it is.
#   bash zebra-gui/gui.sh check                      After editing: build, run every script and random
#                                                    input, and say what changed since `base`.
#   bash zebra-gui/gui.sh run <script|commands>      Run one script with no window; rebuilds if stale.
#        [--scene default|empty] [--out <dir>]       Output goes to target/gui/run unless --out.
#   bash zebra-gui/gui.sh snap <label>               Run every scripts/*.gui into target/gui/<label>.
#   bash zebra-gui/gui.sh diff <label> <label>       What differs between two snaps: screens and lint.
#   bash zebra-gui/gui.sh lint                       Every visual fault the scripts' screens have now.
#   bash zebra-gui/gui.sh bench                      Frame timings from scripts/bench.gui.
#   bash zebra-gui/gui.sh dup [min lines]            Repeated runs of code in src/.
#   bash zebra-gui/gui.sh pace [seconds]             Opens a real window and times its frames.
#   bash zebra-gui/gui.sh build                      Build zebra-gui alone (release-iter).
#   bash zebra-gui/gui.sh test [filter]              The crate's tests.
#   bash zebra-gui/gui.sh window                     Open the real window on mock data.

here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
gui_out="$here/target/gui"
exe="$here/target/release-iter/zebra-gui"
[ -f "$exe.exe" ] || [ "$OS" = "Windows_NT" ] && exe="$exe.exe"

# The machine-wide build lock, on machines that have one.
locked() {
    local name=$1; shift
    if [ -f "$root/../run_one.sh" ]; then bash "$root/../run_one.sh" "$name" "$@"; else "$@"; fi
}

# Git Bash puts its own link.exe ahead of MSVC's, so the batch files get a PATH without Git's
# directories. Only the inner command: the lock script itself needs them.
ph() {
    local bat=$1; shift
    if [ "$OS" = "Windows_NT" ]; then
        local p
        p="$(printf %s "$PATH" | tr ':' '\n' | grep -vE '^(/usr/bin|/bin|/mingw64/bin|/usr/local/bin|/c/Program Files/Git/.*)$' | paste -sd:)"
        CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-5} locked "Gui-$bat" env PATH="$p" cmd //c "$(cygpath -w "$root/$bat.bat") zebra-gui Release Win64 $*"
    else
        local sub=build; [ "$bat" = phest ] && sub=test
        (cd "$here" && RUSTFLAGS=-Awarnings cargo $sub --profile release-iter "$@")
    fi
}

build() {
    mkdir -p "$gui_out"
    local start=$SECONDS
    if ph phuild > "$gui_out/build.log" 2>&1; then
        echo "built in $((SECONDS - start)) s"
    else
        grep -E -A12 '^(error|run_one: FAILED)' "$gui_out/build.log" | head -60
        echo "BUILD FAILED after $((SECONDS - start)) s; full log: $gui_out/build.log"
        return 1
    fi
}

stale() {
    [ -f "$exe" ] || return 0
    [ -n "$(find "$here/src" "$here/assets" "$here/Cargo.toml" "$here/build.rs" "$root/wallet/src" "$root/wallet/Cargo.toml" \
        "$root/clay-rs/src" "$root/clay-rs/clay.h" -type f -newer "$exe" 2>/dev/null | head -1)" ]
}

fresh() { if stale; then build; fi; }

# run_script <scene> <out dir> <script file or commands>
run_script() {
    mkdir -p "$2"
    rm -f "$2/shots.txt" "$2/lint.txt"
    ZEBRA_GUI_SCRIPT="$3" ZEBRA_GUI_OUT="$2" "$exe" --scene "$1"
}

# snap <label>: every script but the benchmark, each into its own directory.
snap() {
    rm -rf "$gui_out/$1"
    local failed=0 shots=0 script name scene log
    for script in "$here"/scripts/*.gui; do
        name="$(basename "$script" .gui)"
        [ "$name" = bench ] && continue
        scene=default; grep -q '^# scene empty' "$script" && scene=empty
        if ! log="$(run_script "$scene" "$gui_out/$1/$name" "$script" 2>&1)"; then
            failed=$((failed + 1))
            echo "$name: FAILED"; echo "$log" | grep -vE '^(shot|film|lint:|settle: still after) ' | tail -15
        fi
        echo "$log" > "$gui_out/$1/$name/log.txt"
        shots=$((shots + $(echo "$log" | grep -c '^shot ')))
    done
    echo "snap $1: $shots screenshots in $gui_out/$1, $failed scripts failed"
    [ $failed -eq 0 ]
}

# diff_snaps <label> <label>
diff_snaps() {
    local result=0 dir name log
    for dir in "$gui_out/$2"/*/; do
        name="$(basename "$dir")"
        [ -f "$dir/shots.txt" ] || continue
        if ! log="$("$exe" diff "$gui_out/$1/$name" "$gui_out/$2/$name" 2>&1)"; then
            result=1
            echo "$log" | sed "s|^|$name: |"
        fi
    done
    [ $result -eq 0 ] && echo "no screen or lint finding differs between $1 and $2"
    return $result
}

cmd=$1; shift
case "$cmd" in
    build) build ;;
    run)
        scene=default; out="$gui_out/run"
        while [ $# -gt 1 ]; do
            case "$1" in
                --scene) scene=$2; shift 2 ;;
                --out) out=$2; shift 2 ;;
                *) break ;;
            esac
        done
        [ $# -eq 1 ] || { echo "usage: gui.sh run [--scene default|empty] [--out <dir>] <script|commands>"; exit 2; }
        fresh || exit 1
        run_script "$scene" "$out" "$1"
        ;;
    snap)
        [ $# -eq 1 ] || { echo "usage: gui.sh snap <label>"; exit 2; }
        fresh || exit 1
        snap "$1"
        ;;
    diff)
        [ $# -eq 2 ] || { echo "usage: gui.sh diff <label> <label>"; exit 2; }
        diff_snaps "$1" "$2"
        ;;
    check)
        fresh || { echo "check: FAIL, it does not build"; exit 1; }
        bad=""
        snap now || bad="$bad a script failed;"
        if log="$(run_script default "$gui_out/now/monkey" "monkey ${1:-400}" 2>&1)"; then
            echo "random input: no crash ($(echo "$log" | grep '^monkey seed' | cut -d' ' -f1-3))"
        else
            bad="$bad random input crashed or failed;"
            echo "$log" | grep -vE '^(shot|lint:) ' | tail -12
            echo "replay it with: bash zebra-gui/gui.sh run $gui_out/now/monkey/monkey.gui"
        fi
        if [ -d "$gui_out/base" ]; then
            diff_snaps base now || echo "Read each diff_*.png and each new lint finding above, and say which you meant to cause."
        else
            echo "No base to compare with. Run 'gui.sh snap base' before editing to see what an edit changes."
        fi
        if [ -n "$bad" ]; then echo "check: FAIL,$bad"; exit 1; fi
        echo "check: ok"
        ;;
    lint)
        fresh || exit 1
        snap lint > /dev/null
        for f in "$gui_out"/lint/*/lint.txt; do
            [ -f "$f" ] || continue
            sed "s|^|$(basename "$(dirname "$f")")  |; s|\t|  |g" "$f"
        done
        echo "lint: $(cat "$gui_out"/lint/*/lint.txt 2>/dev/null | grep -c '') findings. To see one screen's marked in an image: gui.sh run '<commands to reach it>; lint x'"
        ;;
    bench)
        fresh || exit 1
        locked Gui-bench env ZEBRA_GUI_SCRIPT="$here/scripts/bench.gui" ZEBRA_GUI_OUT="$gui_out/bench" "$exe" --scene default | grep -E '^(bench|FAIL)'
        ;;
    dup)
        fresh || exit 1
        (cd "$here" && "$exe" dup "${1:-8}" src/*.rs)
        ;;
    pace)
        fresh || exit 1
        "$exe" --pace "${1:-5}"
        ;;
    test)
        mkdir -p "$gui_out"
        if ph phest "$@" > "$gui_out/test.log" 2>&1; then
            grep -E '^test result' "$gui_out/test.log"
        else
            grep -E -B2 -A25 '^(error|failures:|---- )' "$gui_out/test.log" | head -80
            echo "TESTS FAILED; full log: $gui_out/test.log"
            exit 1
        fi
        ;;
    window)
        fresh || exit 1
        "$exe" "$@"
        ;;
    *) sed -n '2,18p' "$0"; exit 2 ;;
esac
