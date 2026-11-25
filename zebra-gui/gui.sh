#!/usr/bin/env bash
# The standalone GUI's build, scripted run, checks, benchmark and tests. GUI_DEV.md is the manual.
#
#   bash zebra-gui/gui.sh check                      After editing: build, run every script and random
#                                                    input, and say what your uncommitted change did to
#                                                    the screens. What it compares with records itself.
#   bash zebra-gui/gui.sh run <script|commands>      Run one script with no window; rebuilds if stale.
#        [--scene default|empty] [--out <dir>]       Output goes to target/gui/run unless --out.
#   bash zebra-gui/gui.sh snap <label>               Run every scripts/*.gui into target/gui/<label>.
#   bash zebra-gui/gui.sh diff <label> <label>       What differs between two snaps: screens and lint.
#   bash zebra-gui/gui.sh lint                       Every visual fault the scripts' screens have now.
#   bash zebra-gui/gui.sh style                      Every text size, colour and corner radius in use.
#   bash zebra-gui/gui.sh bench                      Frame timings from scripts/bench.gui.
#   bash zebra-gui/gui.sh dup [min lines]            Repeated runs of code in src/.
#   bash zebra-gui/gui.sh pace [seconds]             Opens a real window and times its frames.
#   bash zebra-gui/gui.sh build                      Build zebra-gui alone (release-iter).
#   bash zebra-gui/gui.sh test [filter]              The crate's tests.
#   bash zebra-gui/gui.sh window                     Open the real window on mock data.

# Bash reads a brace group whole before running it, so editing this file does not break a run
# of it that is waiting on the build lock.
{
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

# What the GUI is built from, as paths from the repository's root.
sources="zebra-gui/src zebra-gui/assets zebra-gui/Cargo.toml zebra-gui/Cargo.lock zebra-gui/build.rs wallet/src wallet/Cargo.toml clay-rs/src clay-rs/clay.h"

stale() {
    [ -f "$exe" ] || return 0
    [ -n "$(cd "$root" && find $sources -type f -newer "$exe" 2>/dev/null | head -1)" ]
}

# An id of what the last commit's screens come from: the sources and the scripts. A commit
# that leaves the GUI alone does not change it. Empty outside a Git checkout.
id_committed() {
    local list
    list="$(git -C "$root" ls-tree -r HEAD -- $sources zebra-gui/scripts 2>/dev/null)"
    [ -n "$list" ] && printf '%s\n' "$list" | git hash-object --stdin | cut -c1-12
}

# Whether the GUI has an uncommitted change. Line endings do not count.
gui_changed() {
    ! git -C "$root" diff --quiet HEAD -- $sources zebra-gui/scripts 2>/dev/null \
        || [ -n "$(git -C "$root" ls-files -o --exclude-standard -- $sources zebra-gui/scripts 2>/dev/null | head -1)" ]
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
    if gui_changed; then echo changed; else id_committed; fi > "$gui_out/$1/id"
    echo "snap $1: $shots screenshots in $gui_out/$1, $failed scripts failed"
    [ $failed -eq 0 ]
}

# With no uncommitted change to the GUI, the screens as they are now are what the next change
# gets compared with, so `base` is recorded without being asked for.
auto_base() {
    local committed; committed="$(id_committed)"
    [ -n "$committed" ] || return 0
    gui_changed && return 0
    [ "$(cat "$gui_out/base/id" 2>/dev/null)" = "$committed" ] && return 0
    snap base > /dev/null && echo "base recorded: the GUI has no uncommitted change, so these screens are what the next change is compared with"
}

# Recording `base` over a good one, from a tree that has a change in it, would hide that
# change from every later comparison without a word.
base_would_lose() {
    local committed; committed="$(id_committed)"
    [ -n "$committed" ] && [ "$(cat "$gui_out/base/id" 2>/dev/null)" = "$committed" ] && gui_changed
}

# diff_snaps <label> <label>
diff_snaps() {
    local log
    if log="$("$exe" diff "$gui_out/$1" "$gui_out/$2" 2>&1)"; then
        echo "no screen or lint finding differs between $1 and $2"
    else
        echo "$log"
        return 1
    fi
}

check_all() {
    fresh || { echo "check: FAIL, it does not build"; return 1; }
    auto_base
    local bad="" log
    snap now || bad="$bad a script failed;"
    if log="$(run_script default "$gui_out/now/monkey" "monkey ${1:-400}" 2>&1)"; then
        echo "random input: no crash ($(echo "$log" | grep '^monkey seed' | cut -d' ' -f1-3))"
    else
        bad="$bad random input crashed or failed;"
        echo "$log" | grep -vE '^(shot|lint:) ' | tail -12
        echo "replay it with: bash zebra-gui/gui.sh run $gui_out/now/monkey/monkey.gui"
    fi
    if [ -d "$gui_out/base" ]; then
        [ "$(cat "$gui_out/base/id" 2>/dev/null)" = "$(id_committed)" ] || echo "note: base was not recorded from the last commit, so the list below is not exactly your uncommitted change"
        if ! diff_snaps base now; then
            echo "Read each image named above, one for each way the screens changed, and each line that says 'new'."
            echo "Report: which changes you meant; for each new thing on screen the form and place you chose, and why (GUI_DESIGN.md); the one image a person should look at."
            echo "This list is kept in $gui_out/check.txt."
        fi
    else
        echo "No base to compare with: the GUI already had uncommitted changes when gui.sh first ran here. 'gui.sh snap base' records the screens as they are now, to compare what you change from here on."
    fi
    if [ -n "$bad" ]; then echo "check: FAIL,$bad"; return 1; fi
    echo "check: ok"
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
        auto_base
        run_script "$scene" "$out" "$1"
        ;;
    snap)
        [ $# -eq 1 ] || { echo "usage: gui.sh snap <label>"; exit 2; }
        if [ "$1" = base ] && base_would_lose; then
            echo "base is left as it is: it holds the screens of the last commit, and recording it now would put your uncommitted change into it."
            echo "'gui.sh check' compares your change with it. To keep the screens as they are now, use another name: gui.sh snap <label>, then gui.sh diff <label> now."
            exit 2
        fi
        fresh || exit 1
        [ "$1" = base ] || auto_base
        snap "$1"
        ;;
    diff)
        [ $# -eq 2 ] || { echo "usage: gui.sh diff <label> <label>"; exit 2; }
        diff_snaps "$1" "$2"
        ;;
    check)
        mkdir -p "$gui_out"
        check_all "$@" 2>&1 | tee "$gui_out/check.txt"
        exit "${PIPESTATUS[0]}"
        ;;
    lint)
        fresh || exit 1
        auto_base
        snap lint > /dev/null
        for f in "$gui_out"/lint/*/lint.txt; do
            [ -f "$f" ] || continue
            sed "s|^|$(basename "$(dirname "$f")")  |; s|\t|  |g" "$f"
        done
        echo "lint: $(cat "$gui_out"/lint/*/lint.txt 2>/dev/null | grep -c '') findings. To see one screen's marked in an image: gui.sh run '<commands to reach it>; lint x'"
        ;;
    style)
        fresh || exit 1
        snap style > /dev/null
        cat "$gui_out"/style/*/style.txt 2>/dev/null \
            | awk -F'\t' '{ uses[$2 "\t" $3] += $4 } END { for (k in uses) print k "\t" uses[k] }' \
            | sort -t"$(printf '\t')" -k1,1 -k3,3nr \
            | awk -F'\t' '
                $1 != kind { if (kind != "") print kind " (" n " values): " line; kind = $1; line = ""; n = 0 }
                { line = line (n ? ", " : "") $2 " x" $3; n++ }
                END { if (kind != "") print kind " (" n " values): " line }'
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
    *) sed -n '2,19p' "$0"; exit 2 ;;
esac
exit $?
}
