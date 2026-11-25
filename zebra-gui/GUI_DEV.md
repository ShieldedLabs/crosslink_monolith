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

**Sizes.** The working targets are a 1920x1080 display at scale 1 and 60 Hz to design for,
and a 1280x720 display at scale 1 to stay whole on. A title bar and a task bar take about
80 px of a display's height, so the two windows to check are 1920x1000 and 1280x640. The
tools call them `mainline` and `minspec`; `MAINLINE` and `MINSPEC` in `src/script.rs` set
them, and nothing else has to change if the targets do. A run starts at `mainline`, so a
`shot` shows what most people see. `check` runs every screen at both sizes, and sweeps the
window from one to the other.

```text
snap now: 34 screenshots in .../target/gui/now, 0 scripts failed
random input: no crash (monkey seed 1795626457009387801)
differs: minspec/main: new text "Hide"; new to press "Hide" 111x32 at 49,228; 14 texts moved 44 px down; 1 went out of view; 29867 px in x 9..200 y 228..600 -> .../now/minspec/diff_main.png
    the same cause changed sizes/minspec, tour/main, tour/faucet_wallet and 9 more
differs: tour/modal_send: 2 texts moved 56 px up; 2 came into view; 6847 px in x 12..421 y 298..876 -> .../now/tour/diff_modal_send.png
14 of 34 screenshots differ, in 2 ways
lint new: minspec/main  margin  Hide  67,270  2 px from the side of the box behind it
    the same fault 12 more times, on 12 more screenshots
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
  is yours to fix or to mean. A fault that shows on many screenshots has one line, so there
  are few: skip none.
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
| `run` | under 1 s | Plus a build if a source file changed, and 18 s to record `base` the first time after a commit. An `explore` of the main screen is 10 s. |
| `snap <label>` | 18 s | Every script, side by side, into `target/gui/<label>/<script>/`. |
| `check` | 25 s | `snap now`, random input, `diff base now`. |
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
| `size <w> <h>`, `size mainline`, `size minspec` | Window size in pixels. `mainline` is 1920x1000 and `minspec` 1280x640. |
| `scale <factor>` | Display scale (1, 1.25, 2). A run starts at `mainline` and scale 1. |
| `include <file>` | The lines of another script beside this one. |
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
| `targets` | List what can be pressed: place, size, the cursor it asks for, and how many pixels change when the pointer is on it. Then the grips: what is dragged to resize a pane, by name. |
| `style` | List the screen's text sizes, text colours, fills and corner radii, with counts. |
| `settle [max frames]` | Draw until three frames change nothing; fail if still changing at max. |
| `film <name> [frames] [x y w h]` | One image of that many frames in a row. |
| `track "text" [frames]` | Where the text is in each frame. |
| `resize <name> <w> <h> [steps]`, `resize <name> minspec [steps]` | Move the window's side to that size a step at a time, as a hand dragging it does, and say what breaks on the way. |
| `drag <name> "grip" <dx> <dy> [steps]` | The same for a pane: press the grip, or a text, or `<x> <y>`, and move the pointer that far a step at a time. |
| `explore <name> [depth] [most]` | Press everything on the screen that can be pressed, and whatever each press brings up, `depth` levels in (2), undoing each press before the next. Say what each one opened, moved and broke, and what undoes it. |
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
| `outside` | Something to press that the window itself cuts off, as in a dialog taller than the window. Nothing scrolls it back, so it cannot be reached. |
| `overflow` | Text wider or taller than the box drawn behind it. |
| `margin` | Text under 4 px (times scale) from the side of its box. |
| `contrast` | Text under 3:1 against what is behind it. |
| `tiny` | Text under 10 px (times scale) tall. |
| `overlap` | Two texts drawn on top of each other. |
| `target` | Something to press that is under 24 px (times scale) wide or tall. |
| `edge` | Something to press that stops 1 to 12 px short of the window's left or right side. In a maximized window those are the screen's sides, where the pointer stops, so a target that reaches one cannot be overshot. The top and bottom are not checked: a title bar and a task bar lie beyond them. |

The mock screens have many findings as they stand; `check` reports only new ones. Lint does
not see: thin margins above and below text, misalignment, uneven spacing, or anything about
icons and the block map's shapes. Look at the screenshot for those. It also does not see a
control that lies wholly past the window's side: put the `minspec` screenshot beside the
`mainline` one and see that nothing is missing.

**Style.** Each `shot` also records the text sizes, colours and corner radii on screen.
`check` prints `style new:` for a value no screen of `base` used: a new word in the design's
vocabulary, or a slip. `GUI_DESIGN.md` says which is which.

**Motion.** `settle` prints the changed pixels per frame, so an animation that never stops, or
takes three times as long, shows in numbers. `film` shows the frames side by side. `track`
shows a jump or a stutter in one element's path. The mock main screen settles in 86 frames
(1.4 s) at start-up.

**Resizing.** `resize` moves the window's side, and `drag` a pane's divider, a step at a
time as a hand does. Every step is drawn twice with nothing changed in between: the first
frame is what a person sees mid-drag, the second is what that size comes to rest at.

```text
bash zebra-gui/gui.sh run 'settle 300; resize narrower 1280 1000'
bash zebra-gui/gui.sh run 'size minspec; settle 300; targets; drag left "Left Pane Divider" -300 0'
```

```text
resize narrower: window 1920x1000 -> 1280x1000 in 24 steps; faults the change brings, at each step: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1 3 3 3 5 5 5 5
resize fault overlap  first at 1467x1000 on "cTAZ"; on 6 texts in all; worst on "cTAZ" at 1360x1000: on top of "Fee:"
resize jump  from 1494x1000 to 1467x1000: 3 texts moved further than the window's side did, most of all "-1.000 cTAZ" by -7,-9 px
drag trail: at 18 of 24 steps part of the screen follows a frame late: the frame after, with nothing changed, moved 4 texts, ...
resize narrower: no new fault until 1494x1000, then 6 by 1280x1000 (.../narrower.txt); 1 jumps; 0 of 24 steps follow late; ...
resize narrower.png: the window at 1920x1000 1840x1000 ... 1280x1000, left to right then down, each 1/5 size, a box round each fault the change brings
```

- **`fault`** counts only what the change brought: a fault the first step already had is
  left to `lint`. There is one line for each kind, and `<name>.txt` lists every text. The
  last line's "no new fault until" is the smallest size the screen holds at. To look at one
  size closely: `size 1360 1000; lint x`.
- A sweep from `mainline` to `minspec` should bring no fault at all: people use every size
  between the two.
- **`jump`** is a reflow: texts that moved further than the side or the pointer that pushed
  them, as when a line wraps and everything under it drops. One is how a wrap looks. Several
  close together, or one that takes a control out from under the pointer, is a layout that
  needs a fixed place for that thing.
- **`trail`** means the first frame at a size was not the layout that size comes to rest at,
  so part of the screen lags the hand. One frame late is hard to see; a part that goes on
  moving after the drag stops is not. Follow the sweep with `settle` to time that.
- **The image** is eight of the steps side by side. Read it: it shows what the numbers
  cannot, such as a pane that keeps its width while the one beside it is crushed.
- Sweep one axis at a time and start from a size where the screen is sound, after a `settle`.
  `targets` lists the grips `drag` can take hold of. `drag` also takes a text, or `x y`.
- `check` runs `scripts/resize.gui`, so a change that moves a breakpoint or brings a fault
  shows there as a film that `differs` and as `lint new`.

What a real window does while its border is dragged (whether the last frame is stretched, or
a strip is left unpainted) is the operating system's and softer_gui's, and no script sees it.

**Pressing things.** A screenshot is of a screen at rest, and this GUI does not stay at
rest: a menu drops down, a row of the history expands, a filter opens a panel, and each
pushes the rest of the page about. `explore` presses everything that can be pressed, one
thing at a time. After each press it parks the pointer, waits for the screen to stop
moving, and reads it. What the press brought up it presses too, one level in. It undoes
each press before the next as a person would: the same thing again, the tab beside it, esc,
a close button it has seen work, or three seconds of waiting.

```text
bash zebra-gui/gui.sh run 'settle 300; explore main'
bash zebra-gui/gui.sh run 'click "Send"; explore send'       Inside a dialog.
```

```text
explore "Network Info": new text "0", "3", "4", "8" and 15 more; 1 text moved by -87,-585 px, 2 moved otherwise; pressing it again undoes it -> .../main_network_info.png
explore "Finalizer Filters": new text "15 ", "Connected under" ...; 37 texts moved 128 px down, 1 moved otherwise; pressing it again undoes it -> .../main_finalizer_filters.png
explore "Finalizer Filters": brings 4 faults: target "Voted at latest height" 348x14 px to click, under 24 px, margin "seconds ago" 2 px from the side of the box behind it, ...
explore "Faucet Wallet": new text "993.250 cTAZ" ...; 76 texts moved 68 px up; pressing "Your Wallet" beside it undoes it -> .../main_faucet_wallet.png
explore "Jump To Height...": new text "Jump To Height" ...; esc does not undo it once things in it were pressed; pressing <e81f> at 1172,411 in it undoes it -> .../main_jump_to_height.png
explore "Received @ 10": new text "00000000..00000010" ...; nothing undoes it: not esc, not pressing it again, not what is beside it or closed it before, not waiting -> .../main_received_10.png
explore all 4 that a second press closes, open at once: ... -> .../main_all_open.png
explore main: 8 presses led back to a screen passed on the way, as a close button does: "Send" > <e81f> at 332,325, ...
explore main: 3 presses changed how something is drawn and moved no text: "Jump To Height..." > "Enter PoW height..." (18370 px, and a second press did not put it back), ...
explore main: 28 presses changed nothing on screen: "Your Wallet", "Send" > "Paste Address", ...
explore main: closing all that was open at once, last first, did not bring the screen back: new text "00000000..0000000d", ...
explore main: pressed 64 things at 1920x1000, 2 deep, of 48 on screen: 26 opened another screen, 8 led back, 3 only redrew, 28 changed nothing; left out 36 alike in a list and 17 disabled; what opened brought 20 faults on 6 screens -> .../main.txt
```

- **One line for each press that led to another screen:** what it brought, what that moved,
  and what undoes it. `"Send" > "Paste Address"` is "Paste Address" pressed in what "Send"
  opened. The image is the whole window in that state: read the one for each state your
  change touches. Many texts moved is a reflow: ask whether it had to be.
- **`brings N faults`** are the lint findings the screen did not have before the press: a
  menu the window cuts off (`outside`), a panel drawn over text (`overlap`). They are kept
  in lint under the screenshot's name, so `check` says `lint new` when your change adds one.
- **`nothing undoes it`** is right for a slider, or for a row that moves the block map. For
  a menu, a panel or a dialog it is a fault: a person cannot get out of it either. `esc
  does not undo it` on a dialog is a fault too.
- **`all N that a second press closes, open at once`** is the page with every menu and
  panel open together, where there is least room. Read its image. `the N of M` means some
  shut as others opened, as one menu shuts another.
- **`closing all ... did not bring the screen back`** says what stayed changed after they
  were closed again, last first. On the main screen it is the view lock: while it is on
  the block map moves on, and switching it off leaves the map where it got to. Anything
  else there is a panel that stays open, or that leaves something behind.
- **`changed nothing on screen`** lists presses with no effect: a button the mock data
  gives nothing to do, or a broken one. Find the ones you added in it.
- **`only redrew`** is a switch that shows its state by colour alone, or a text field that
  took the caret. A switch is pressed a second time to put it back, and the line says if
  that did not: right for a field, a fault for a switch.
- **`N things were not pressed`** names what it could not get back to, after a press that
  nothing undid changed the screen they were on. Press those by hand.
- **`the screen was still changing 90 frames after a press`** is an animation that does not
  stop. What explore read there may be a frame from the middle of it.
- Of four or more things of one size in a column, as the rows of a list, it presses the
  first and the last. What is disabled is not pressed. The last line counts both.
- It presses, and does not type. What needs an amount or an address typed first does
  nothing here: script that by hand, as `tour.gui` does.
- A press nothing undoes leaves its mark on the screens after it. The other presses are
  still made and still judged, each against the screen just before it.
- `<name>.txt` holds every line in full, and under them every press in order, each with the
  command that makes it:

  ```text
  click 269 192   "Edit Stake"
  click 196 269   "Edit Stake" > "Unknown (ra…"
  click 196 269     to undo it: again
  key esc           to undo it
  ```

  To see one state for yourself, run the commands down to it with `gui.sh run`, a
  `move 4 4` and a `settle` after each, then `shot`, `lint` or `targets`.
- `check` runs `scripts/explore.gui` and `scripts/explore_minspec.gui`, the main screen at
  each size, and prints the last line of each. Every state they reach is a screenshot like
  any other: a change to an open menu shows as `differs`, a new menu as `new screenshot`.
- A dialog or a pane you add may hold things the main screen's run does not reach, two
  levels in. Give it its own lines in a script: `click "Stake"; explore stake`.

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
microseconds), at `mainline` and at 3840x2160, and what share of a 60 Hz frame the 95th
percentile takes. Over 100% there, the screen cannot keep up with the display. Run it before
and after a speed change and compare medians; a difference under 5% is noise.

```text
bench 200 frames 1920x1000 us: layout [min 626 med 732 p95 914 max 1325] total [min 5954 med 6861 p95 7469 max 7664]; p95 is 44% of a 60 Hz frame
```

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
- Does it hold at `minspec`, and at `scale 1.25` and `scale 2`? `scripts/minspec.gui` and
  `scripts/sizes.gui` show.
- Does it hold while the window or its pane is made smaller? `resize` and `drag` show.
- Does it hold with what is on the page open: each menu, each expanded row, all of them at
  once? Does everything you added close again? `explore` shows.
- Does anything depend on frame count where it should depend on time?
- Did you add state that must be reset when a dialog closes or the wallet changes?
- Is each new id unique? Two widgets with one id share hover and press.

## Where things are

| File | Lines | Holds |
| --- | --- | --- |
| `src/ui.rs` | 5,800 | Every screen and widget. `run_ui` is the whole frame's layout. |
| `src/lib.rs` | 2,800 | The frame loop, input, the rasteriser and its worker threads. |
| `src/viz_gui.rs` | 2,450 | The block map, and the messages to and from the node. |
| `src/script.rs` | 3,800 | The scripted window, lint, design, motion and resize checks, `explore`, PNG, crash reports. |
| `src/style.rs` | 70 | Named colours, text sizes, control sizes and pane widths. Spaces and times belong here too. |
| `src/main.rs` | 140 | The standalone program and its mock wallet data. |
| `scripts/tour.gui` | | Every screen and dialog, at `mainline`. |
| `scripts/minspec.gui` | | The same tour at `minspec`. |
| `scripts/sizes.gui` | | The main screen at `minspec`, at `mainline` and on three sharper displays. |
| `scripts/motion.gui` | | Start-up, scrolling, a dialog, the block map. |
| `scripts/resize.gui` | | The window from `mainline` down to `minspec`, and each pane's divider dragged at `minspec`. |
| `scripts/explore.gui` | | Everything on the main screen pressed, at `mainline`. |
| `scripts/explore_minspec.gui` | | The same at `minspec`. |
| `scripts/bench.gui` | | What `bench` times. |

Layout is by `clay` (`../clay-rs`); the window, input and pixels are by `softer_gui`, a
separate repository pinned by revision in `Cargo.toml`. A change there needs a push to that
repository and a new revision here.

`zebra-gui` must not depend on a node crate. Data the GUI needs from the node comes in
through the messages in `viz_gui.rs`.

## Known faults

Found with these tools and not yet fixed. Each is a good first task.

- **Nothing stops the window shrinking below `minspec`.** softer_gui has no least window
  size; below it the block map gives up its room first and then the panes clip.
- **Block-map text shows in the gap under the left pane** at 1280x1000 and narrower
  (`outside` in `resize narrower`). The block map pans, so it is not lost, but text of a
  block hidden behind a pane should not show at all.
- **Controls at the side panes' outer edges stop 12 px short of the window's sides**
  (`edge`): the panes' margin. Reaching the side needs their hit areas to run past the pane.
- **A dropped transaction's row is drawn at 2.3:1** against its background (`contrast`), on
  purpose to show it is gone; a clearer form would pass the lint.
- **The block inspector's resize handle resizes nothing.** A drag on it pans the block map
  behind it.
- **Buttons disagree about the pointer.** 14 call sites of `button_ex` ask for the hand
  pointer; every plain `Context::button` asks for none. Pick one rule.
- **Two block hash labels are drawn on top of each other** in the block map (`overlap`).
- **"Finalizer Filters", open, runs to the window's right side**: "seconds ago" ends 2 px
  from it, and "Voted at latest height" is a 14 px tall thing to press (`explore`).
- **At `minspec`, the open "Network Info" panel lies over the PoW bar and the block map's
  text**, and with "Finalizer Filters" open too, "Jump To Height..." and the last history
  row are cut off by the window's bottom (`explore`, `minspec_all_open.png`).
- **Esc does not close "Jump To Height..." once its text field was pressed.** Its close
  button does.
- **A bond's row in "Edit Stake", expanded, has two things to press under 24 px**, 16x18
  and 17x18. At `minspec` the expanded row brings a scroll bar that moves the whole list
  6 px left. The row stays expanded when the dialog is closed and opened again.
- **Unmuting sets the volume to 100%**, not to what it was before the mute.
- **"Receive cTAZ" turns grey when pressed and stays so**, with nothing on screen saying
  why.
- 321 of 9,700 lines of code are a second copy of another run of 8 or more lines (`dup`).
