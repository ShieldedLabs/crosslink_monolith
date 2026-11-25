# Working on the GUI

`zebra-gui` builds and runs on its own, without a node. `gui.sh` builds it, drives it from a
script with no window, and reports in text and small images. One edit, checked, costs about
30 seconds.

## The loop

```text
bash zebra-gui/gui.sh snap base     Once, before the first edit.
  ...edit...
bash zebra-gui/gui.sh check         After each edit.
```

`check` rebuilds if a source file is newer than the binary, runs every script in `scripts/`,
feeds 400 steps of random input, and compares every screenshot and every lint finding with
`base`. It ends in `check: ok` or `check: FAIL, <why>`.

```text
snap now: 19 screenshots in .../target/gui/now, 0 scripts failed
random input: no crash (monkey seed 1795623091612353501)
tour: differs: main.png 5120 px in x 40..420 y 250..300 -> .../now/tour/diff_main.png
tour: lint new: main  margin  Send  67,270  2 px from the side of the box behind it
check: ok
```

`check: ok` means nothing crashed and no script's `expect` failed. It does not mean the
change looks right: read each `diff_*.png` (the changed region, before on the left, after on
the right) and each `lint new:` line, and decide whether you meant it.

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
| `run` | under 1 s | Plus a build if a source file changed. |
| `snap <label>` | 5 s | Every script, into `target/gui/<label>/<script>/`. |
| `check` | 11 s | `snap now`, random input, `diff base now`. |
| `lint` | 5 s | Every finding on every screenshot, one line each. |
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

It also prints how many text sizes and colours the screen uses. The mock screens have about
30 findings each as they stand, 500 across the scripts; `check` reports only new ones. Lint does not see: thin margins
above and below text, misalignment, uneven spacing, small click targets, or anything about
icons and the block map's shapes. Look at the screenshot for those.

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
| `src/script.rs` | 1,500 | The scripted window, lint, motion checks, PNG, crash reports. |
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
