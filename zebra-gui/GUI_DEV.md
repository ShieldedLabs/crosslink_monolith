# Working on the GUI

`zebra-gui` builds and runs on its own, without a node. `gui.sh` builds it, drives it from a
script with no window, and reports in text and small images. One edit, checked, costs about
30 seconds.

This file is how to build and check. `GUI_DESIGN.md` is what to build: read it before adding
or changing anything a person sees.

## The loop

```text
bash zebra-gui/gui.sh run 'shot before'   Before the first edit: look at the screen as it is.
  ...edit...
bash zebra-gui/gui.sh check               After each edit.
```

Whenever `gui.sh` runs and the GUI has no uncommitted change, it records every screen under
`base`. That is what `check` compares a change with, so run `gui.sh` once before the first
edit. Never record `base` yourself after editing: your change would go into it, and `check`
would report that nothing differs. `gui.sh` refuses that while `base` is good. It asks for
`gui.sh snap base` only when it first ran in a tree that had GUI changes already.

`check` rebuilds if a source file is newer than the binary, runs every script in `scripts/`,
feeds 400 steps of random input, and says what differs from `base`. It ends in `check: ok`
or `check: FAIL, <why>`, and keeps its whole output in `target/gui/check.txt`.

```text
snap now: 19 screenshots in .../target/gui/now, 0 scripts failed
random input: no crash (monkey seed 1795626457009387801)
differs: sizes/800x600: new text "Hide"; new to press "Hide" 111x32 at 49,228; 14 texts moved 44 px down; 1 went out of view; 29867 px in x 9..200 y 228..600 -> .../now/sizes/diff_800x600.png
    the same cause changed sizes/1024x600, tour/main, tour/faucet_wallet and 9 more
differs: tour/modal_send: 2 texts moved 56 px up; 2 came into view; 6847 px in x 12..421 y 298..876 -> .../now/tour/diff_modal_send.png
14 of 19 screenshots differ, in 2 ways
lint new: tour/main  margin  Hide  67,270  2 px from the side of the box behind it
style new: text size 13 px (first in tour/main, used 1 times)
check: ok
```

`check: ok` means nothing crashed and no script's `expect` failed. It does not mean the
change looks right. Read the report:

- **`differs`** has one line for each cause, not for each screenshot. `new text` and `new to
  press` are what no screenshot had before, `gone text` what none has now. `moved`, `came
  into view` and `went out of view` are what followed from that. A change that moves the
  whole page shows here as a large count of texts moved: ask whether it had to.
- **The image** a line names is of the smallest window that cause changed, where there is
  the least room: the changed region before, after, and with the changed pixels marked. Read
  each one. The other screenshots' images lie beside it as `diff_<name>.png`.
- **`lint new`** and **`style new`** are findings and values no screenshot had before. Each
  is yours to fix or to mean.
- A line that says `no text and nothing to press changed` is a change of drawing only: a
  colour, a shape, a size. A film has no record of its texts, and says so.

To try one thing, give `run` commands in place of a script file, separated by `;`:

```text
bash zebra-gui/gui.sh run 'click "Send"; shot send; lint send'
bash zebra-gui/gui.sh run 'find Wallet'                          Text on screen, with positions.
bash zebra-gui/gui.sh run --scene empty 'shot first_run'         A wallet with nothing in it.
```

Output goes to `zebra-gui/target/gui/run/`. Read the PNG a command names; crop with
`shot name x y w h` so the image is small.

## What each command costs

| Command | Time | What it does |
| --- | --- | --- |
| `build` after an edit | 16 to 30 s | 5 s of compiling; the rest is the machine's build lock. |
| `build` from nothing | 8 min | Happens when the target directory was cleaned. |
| `run` | under 1 s | Plus a build if a source file changed, and 5 s to record `base` the first time after a commit. |
| `snap <label>` | 5 s | Every script, into `target/gui/<label>/<script>/`. |
| `check` | 11 s | `snap now`, random input, `diff base now`. |
| `lint` | 5 s | Every finding on every screenshot, one line each. |
| `style` | 5 s | Every text size, colour and corner radius the screens use, with counts. |
| `bench` | 10 s | Frame timings; takes the build lock so nothing else skews it. |
| `dup` | under 1 s | Repeated runs of code in `src/`. |
| `pace [seconds]` | the seconds | Opens a real window. The only command that does. |
| `test [filter]` | 30 s | The crate's tests. |

Start anything over five seconds in the background and keep reading code meanwhile.

## Script commands

A script is a text file under `scripts/`, one command a line, `#` for comments. A run draws
frames only when a command needs one, each frame 1/60 s of animation after the last, so a run
gives the same pixels every time.

| Command | Meaning |
| --- | --- |
| `size <w> <h>`, `scale <factor>` | Window size in pixels; display scale (1, 1.25, 2). |
| `frame [n]` | Draw n frames. |
| `move <x> <y>` | Move the pointer. |
| `click "text" [nth]`, `click <x> <y>` | Move to the text (exact match wins) and press. |
| `hover "text"`, `hover <x> <y>` | Move there and draw a frame. |
| `down <key>`, `up <key>`, `key <key>` | `a`..`z`, `0`..`9`, `esc`, `enter`, `tab`, `ctrl`, `shift`, `f1`.., `left`.., `mouse`, `mouseright`. |
| `type some text` | Text input. |
| `wheel <dy> [dx]`, `scroll <dx> <dy>`, `zoom <d>` | Mouse wheel clicks; touchpad pixels; pinch. |
| `expect "text"`, `expect_not "text"` | Fail unless the text is, or is not, on screen. |
| `expect_cursor <shape>` | Fail unless the widget under the pointer asks for that shape (`default`, `pointer`, `ewresize`). |
| `find "text"`, `dump [file]` | Print matching texts, or all: `x y height colour text`. |
| `shot <name> [x y w h]` | Save `<name>.png`, and lint the screen under that name. |
| `lint [name]` | Print the screen's findings and save `lint_<name>.png` with each one boxed. |
| `squint [name]` | Save `squint_<name>.png`, the screen shrunk to its masses, and list the text that pulls the eye first. |
| `targets` | List what can be pressed: place, size, the cursor it asks for, and how many pixels change when the pointer is on it. |
| `style` | List the screen's text sizes, text colours, fills and corner radii, with counts. |
| `settle [max frames]` | Draw until three frames change nothing; fail if still changing at max. |
| `film <name> [frames] [x y w h]` | One image of that many frames in a row. |
| `track "text" [frames]` | Where the text is in each frame. |
| `bench [frames]`, `bench_idle [frames]` | Time full redraws; time frames where nothing changed. |
| `monkey [steps] [seed]` | Random input. Writes its commands to `monkey.gui` as it goes. |
| `log text`, `sleep <ms>` | Print; wait. |

`click` and `key` draw the frames they need, so they hide how fast the screen answers. To
time a response, use `down mouse`, `up mouse`, then `settle`.

A script whose first lines include `# scene empty` runs on the empty wallet.

## What the checks see

**Lint** reads the text and boxes the frame drew, and the pixels behind them:

| Kind | Fault |
| --- | --- |
| `clipped` | Text cut off at the side by its container. |
| `overflow` | Text wider or taller than the box drawn behind it. |
| `margin` | Text under 4 px (times scale) from the side of its box. |
| `contrast` | Text under 3:1 against what is behind it. |
| `tiny` | Text under 10 px (times scale) tall. |
| `overlap` | Two texts drawn on top of each other. |
| `target` | Something to press that is under 24 px (times scale) wide or tall. |
| `edge` | Something to press that stops 1 to 12 px short of the window's left, right or bottom side. Reaching the side makes it impossible to overshoot in a full-size window. |

The mock screens have many findings as they stand; `check` reports only new ones. Lint does
not see: thin margins above and below text, misalignment, uneven spacing, or anything about
icons and the block map's shapes. Look at the screenshot for those.

**Style.** Each `shot` also records the text sizes, colours and corner radii on screen.
`check` prints `style new:` for a value no screen of `base` used: a new word in the design's
vocabulary, or a slip. `GUI_DESIGN.md` says which is which.

**Motion.** `settle` prints the changed pixels per frame, so an animation that never stops, or
takes three times as long, shows in numbers. `film` shows the frames side by side. `track`
shows a jump or a stutter in one element's path. The mock main screen settles in 78 frames
(1.3 s) at start-up and 12 frames after a wheel click.

**Crashes.** A scripted run reports its own:

```text
CRASH access violation reading address 0x1c8 (script line 14: size 300 200)
    zebra_gui::main_thread_run_program  src\lib.rs:2285
PANIC index out of bounds ... (script line 9: click "Stake")
HANG the frame loop has not come back for 30 s (script line 22: key esc)
```

Random input that crashes leaves `target/gui/now/monkey/monkey.gui`; `gui.sh run <that file>`
replays it. A crash that depends on thread timing may need several replays.

**Speed.** `bench` prints layout and whole-frame times (min, median, 95th percentile, max, in
microseconds). Run it before and after a speed change and compare medians; a difference
under 5% is noise.

**A real window.** `pace 5` shows every frame for five seconds and prints the gaps between
them. It measures the machine it runs on: say which display it was.

**Repeated code.** `dup` lists runs of 8 or more lines that appear more than once. Run it
after adding a screen or a widget; if your change is in the list, fold the copies together.

## What nothing here checks

Say these are unchecked unless a person looked, or you ran a real window and say what you saw:

- The pointer's shape on screen, the window title and icon, the clipboard, sound.
- macOS and Linux. The scripted run is the same code on each, but only Windows has run it.
- The GUI inside zebrad. `zebra-crosslink/zebra-crosslink/src/viz2.rs` feeds it real blocks;
  after changing a type or function that file uses, also run `pheck.bat zebra-crosslink Release`.
- Whether it feels right: animation speed, timing, the look of a design.

## Review before you report

The tools find what is broken. These they cannot find; check them by reading your diff:

- Does a number in the code (a size, a delay, a colour) repeat one that exists? Use the
  existing one.
- Does the new widget follow its neighbours: same padding, same text size, same hover and
  pressed colours, pointer cursor if it can be clicked?
- Does it work at `scale 1.25` and `scale 2`, and at 800x600? `scripts/sizes.gui` shows.
- Does anything depend on frame count where it should depend on time?
- Did you add state that must be reset when a dialog closes or the wallet changes?
- Is each new id unique? Two widgets with one id share hover and press.

## Where things are

| File | Lines | Holds |
| --- | --- | --- |
| `src/ui.rs` | 5,800 | Every screen and widget. `run_ui` is the whole frame's layout. |
| `src/lib.rs` | 2,800 | The frame loop, input, the rasteriser and its worker threads. |
| `src/viz_gui.rs` | 2,450 | The block map, and the messages to and from the node. |
| `src/script.rs` | 1,750 | The scripted window, lint, design and motion checks, PNG, crash reports. |
| `src/style.rs` | 25 | Named colours. Named sizes, spaces and times belong here too. |
| `src/main.rs` | 140 | The standalone program and its mock wallet data. |
| `scripts/tour.gui` | | Every screen and dialog. |
| `scripts/sizes.gui` | | The main screen at six window sizes and scales. |
| `scripts/motion.gui` | | Start-up, scrolling, a dialog, the block map. |
| `scripts/bench.gui` | | What `bench` times. |

Layout is by `clay` (`../clay-rs`); the window, input and pixels are by `softer_gui`, a
separate repository pinned by revision in `Cargo.toml`. A change there needs a push to that
repository and a new revision here.

`zebra-gui` must not depend on a node crate. Data the GUI needs from the node comes in
through the messages in `viz_gui.rs`.

## Known faults

Found with these tools and not yet fixed. Each is a good first task.

- **Animations run at the wrong speed on displays that are not 60 Hz.** The frame loop
  assumes 60 Hz (`frame_interval_milli_hertz` in `lib.rs` is never updated; the port to
  softer_gui dropped the line that read the display's rate). `pace` on a 240 Hz display
  measured 172 frames a second, so motion runs about three times too fast there, and the F3
  overlay says "Rate: 60 hz".
- **The pointer never changes shape.** Widgets choose one and `lib.rs` drops it:
  softer_gui has no call to set it.
- **Buttons disagree about the pointer.** 14 call sites of `button_ex` ask for the hand
  pointer; every plain `Context::button` asks for none. Pick one rule.
- **Finalizer keys are cut off** in the Finalizers panel (`clipped`, 7 lines).
- **Two block hash labels are drawn on top of each other** in the block map (`overlap`).
- **Inactive tabs are faint**: 2.7:1 against their background (`contrast`).
- **Labels of 7 to 9 px** on the staking bars (`tiny`).
- 321 of 9,700 lines of code are a second copy of another run of 8 or more lines (`dup`).
