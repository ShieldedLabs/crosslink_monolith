# Designing the GUI

Whoever adds a feature to the GUI is its designer. A request that says only "add X" still
asks: what does the person see first, what do they do with their hand, what happens when it
goes wrong, and does it look like it belongs to a program someone cared about. Nobody comes
after you to do that part.

The screens as they stand are not the standard. Where they break what is written here, do not
copy them. This file is the standard, and it is short enough to read before each feature.

## Before the code

1. **Say what the person is doing.** Three lines: what they came to do, what they must see to
   decide, and the one action most of them take.
2. **Think of two or three layouts that really differ**, not one layout with variations. Pick
   by the principles below, not by which is nearest to the code that exists. For a new screen
   or a new flow, first look up how the best programs of any decade solved the same problem,
   and say in the report what you took from where.
3. **Give each control the form its job calls for** (see "Each control fits its job").
4. **Build it, then look at it as a stranger would** (see "Looking at your own design").
5. **Report the design**: what you chose and why, in three bullets, and the one image a
   person should look at.

## The hand

- **Big and near is fast; small and far is slow.** What is used most is large and close to
  where the pointer already is. What is dangerous is small, apart, and never beside what is
  routine.
- **The sides and corners of the screen are the largest targets there are.** The pointer
  stops at them, so a hand can be thrown at one without aiming. A control used often belongs
  against a side or in a corner, and must reach all the way to it: a strip of dead pixels
  between a button and the side throws the whole advantage away.
- **Keep the hand where it is.** The next step appears where the last one ended: the result
  beside the control that made it, the confirmation beside the thing confirmed.
- **Nothing moves under the pointer.** Layout does not shift between press and release, or
  when data arrives. Leave room for what is still loading.
- **A menu that opens beside its parent stays open while the pointer travels toward it.**
  Test the pointer's path against the triangle from where it was to the near corners of the
  submenu; close only when it leaves that triangle. Never make a person steer down a corridor.
- **The keyboard reaches everything.** Enter does the main action, Esc backs out, Tab follows
  reading order, the focus is visible. A dialog opens with its first field focused and that
  field's contents selected.
- **Wheel, drag and scroll act on what is under the pointer.**

## The eye

- **Every screen has one first thing.** Rank what is on it, then make size, weight, contrast
  and position follow the rank. Things of unequal importance look unequal by a factor of two
  or three, not by 14 px against 16.
- **People read from the top left and finish at the bottom right.** What they need first goes
  where reading starts; the action that ends the task goes where reading ends.
- **Group by nearness first**, then by alignment, then by a shared background, and by a drawn
  line last. The space inside a group is smaller than the space between groups. Every border
  is ink that carries no information, and two lines make a third shape between them.
- **Every edge lines up with another edge, or has a reason not to.** Numbers that are
  compared sit in a column, right-aligned, in digits of equal width, with the unit dimmer
  than the number.
- **Colour means something or is not there.** One colour means "act here". Red means loss or
  no way back. Nothing is told by colour alone: one man in twelve cannot tell red from green.
- **Dim text is for what matters less, never for elegance.** Body text keeps 4.5:1 against
  its background, large text 3:1.
- **Dense is good when it is ordered.** A person who uses this daily reads a tight table
  faster than a spaced-out stack of cards. Space separates groups; it is not decoration. Do
  not put behind a click what fits on the screen.
- **Movement pulls the eye harder than anything.** Use it to show where a thing came from or
  went, or that something changed. Under 200 ms, never looping, never in the way of input,
  and always cut short by the next input.

## The head

- **Show, do not ask to remember.** The choices are visible. So is the state: which network,
  whether it is in sync, what can be spent now and what is still pending.
- **A thing lives in one place.** People find controls by where they were. Do not reorder a
  list while it is being read, and do not remove a control that cannot be used now: disable
  it and say why.
- **No modes where one can be avoided.** Where one cannot, it shows at the spot the person is
  looking at. A dialog that blocks the rest is for when the program cannot go on without
  the answer.
- **Undo beats "Are you sure?".** People confirm by habit, without reading. For what cannot
  be undone, as with sending coins, show the consequence before the act (the amount, to
  whom, the fee, what is left) and make the act itself deliberate: its own control, away
  from the routine ones, named for what it does.
- **Answer every press at once.** The pressed state shows in the same frame. Past a second,
  show that work is going on. Past ten, show how far it is and offer a way out.
- **An error says what happened, what state things are in now, and what to do**, next to
  where it happened, and keeps what the person typed.
- **A button says what it does**: "Send 5 cTAZ", not "OK", "Confirm" or "Submit". One word
  for one thing, everywhere.
- **Fewer choices are faster, but an expert's tools are not hidden.** Put behind "more" only
  what is truly rare.
- **What can be pressed looks pressable before the pointer reaches it.** What cannot, does
  not. A field looks like a field.

## Each control fits its job

A control's form comes from its job: how often it is used, how much it matters, how much harm
it can do, and whether it goes somewhere, does something, switches something, picks one of
several, or takes input. Controls with the same job look the same. Controls with different
jobs look different. Consistency means the first; it never meant one shape for everything.

- A screen region has one main action, and only that one looks like it.
- Before drawing a button, ask whether it should be one. The row itself can be pressed. The
  divider can be dragged. The value can be edited where it stands. A switch, a tab, a link
  and a key are all different things from a button.
- A row of identical buttons has no rank. The person must read every label to find theirs.
- What cannot be undone differs from the routine in shape, in place and in colour.
- An icon stands alone only when everyone knows it. Otherwise it has a label.

## What to avoid

Habits of web pages, wrong on a desktop screen with a mouse:

- Navigation hidden behind a menu button when there is room to show it.
- A rounded card around everything, and wide padding that pushes the content off the screen.
- Grey text on grey. Flat labels that are secretly buttons. Controls that appear only on hover.
- One rounded button shape for every action.
- Messages that vanish on a timer while saying something that matters.
- Everything centred. A dialog for everything. Movement on load that does nothing.

The look of a machine-made interface, which people now spot at once. The fault under all of
it is sameness: it would suit any program, so it says nothing about this one.

- The default typeface at the default sizes, one after the other, with no jump between them.
- A purple or blue gradient; cyan on near-black; glow; blur; gradient text.
- Emoji used as icons. Three equal boxes in a row, each with an icon on top.
- One corner radius and one shadow on everything. Even spacing, so nothing ranks.
- A palette spread thin and timid, with no colour in charge.
- Decoration with no job.

Ask of your screen: would it look the same in any other program? If so, it is not designed yet.

## What this program is

A wallet, and a live view of a chain. The wallet is about money, so it is exact, calm and
plain about consequences. The view is an instrument, so it is dense, quick to read and alive
with what is happening now. Its character comes from its subject, not from ornament: a bond
locked for a time can look locked and show its time running; a block that is final can look
final; an amount is a number someone will check twice.

## Freedom, and keeping it whole

You may add what the design needs: a typeface, an icon, an image, a sound, an animation, a
new kind of control, a new way to draw (a bevel, a shadow, a gradient, a picture), a new idea
of how a task goes. That the code has no example of it is not a reason to hold back.

What keeps this from turning into a jumble is one rule: **a new element takes a job that
nothing had, and is written into the vocabulary with that job.** A second element for a job
that has one is the jumble. Reuse the first, or replace it everywhere.

The vocabulary today:

| Kind | In use | Job |
| --- | --- | --- |
| Typeface | Inter | Interface text. |
| | DejaVu Sans Mono | Hashes, addresses, amounts in columns. |
| | `fontello.ttf` | Icons. |
| | In `assets/`, unused: Source Serif 4, Gohu 11 and 14, Tiny5, 3x3 Mono, Public Pixel | None yet. |
| Colour | `src/style.rs`: red, green, blue, gold, each with a dark | Loss, gain, action, reward. |
| | Greys, as literals in `ui.rs` | Surfaces and text. Not yet named. |
| Text size | 12 sizes on the main screen, as literals | Too many. Aim for five or six with clear jumps. |
| Spacing, corner radius, timing | Literals in `ui.rs` | Not yet named. |

Names live in `src/style.rs`. When you work on a screen, move the sizes, spaces, radii and
times you touch into named constants there, each with a comment saying its job.
`bash zebra-gui/gui.sh style` prints every value in use, and `check` prints `style new:` when
a change brings in a value no screen had. Each such line is either a new word in the
vocabulary, which you add to `style.rs` with its job, or a slip, which you fix.

How to add:

- **A typeface.** Put the file in `assets/`, `include_bytes!` it beside `INTER` in `lib.rs`,
  add a `FontKind`, and extend each match on `FontKind` there. Its licence must allow
  shipping it (SIL OFL, Apache, public domain).
- **An icon.** The icons are glyphs of `fontello.ttf`, named in `src/fontello_icons.rs`.
  Adding one goes through the fontello.com website (`icon-font/README.md`), which a person
  has to do: ask for it and name the icon. Or draw the shape yourself from rectangles and
  lines, or from a glyph of a font you add.
- **An image.** The rasteriser draws rounded rectangles, lines and text. A picture needs a
  new `DrawCommand` in `lib.rs`. Decode the file in `build.rs`, as the window icon is, so the
  program carries no decoder.
- **A way to draw.** Add a `DrawCommand` and its case in the rasteriser. `gui.sh bench` tells
  what it costs.
- **Movement.** Move toward a target by elapsed time, never by frame count; see
  `animate_bc_towards_target` in `viz_gui.rs`. Check it with `settle` and `film`.
- **A sound.** `assets/*.ogg`, played through `audio.rs`.

## Looking at your own design

```text
bash zebra-gui/gui.sh run '<commands to reach the screen>; squint x; targets; lint x; style'
```

- **`squint x`** saves the screen shrunk until only its masses show, and lists the text that
  pulls the eye first. Is that list the order of importance?
- **`targets`** lists everything that can be pressed: where, how big, and whether it shows
  anything when the pointer is on it. Is the most-used one the biggest and the nearest?
- **`lint x`** marks text that is cut off, crowded, faint or tiny, targets under 24 px
  (`target`), and targets that stop just short of the window's side (`edge`).
- **`style`** lists the sizes, colours and radii on the screen. Did you add one? Why?
- **`film`** and **`settle`** show movement frame by frame and how long it takes.
- **`scripts/sizes.gui`** and `--scene empty` show the small window, the large display and
  the wallet with nothing in it. Design the empty, the loading, the failed and the
  very-long-number cases too.

Then answer these, and fix what fails:

1. With the labels covered, can you tell what can be pressed?
2. Are the three things `squint` lists first the three that matter most?
3. How far does the hand go for the commonest action, and how big is what it lands on?
4. What happens on a wrong click? Can it be taken back?
5. Would the screen look the same in any other program?

The tools cannot tell whether it is beautiful, or whether it feels right in the hand. Say so
in the report, and give the person the smallest thing to judge: one image, one question.

## Further reading

- Bruce Tognazzini, "First Principles of Interaction Design", asktog.com.
- Jef Raskin, *The Humane Interface*, on modes and habits.
- Smith, Irby, Kimball, Verplank and Harslem, "Designing the Star User Interface", Byte, April 1982.
- Apple, *Human Interface Guidelines*, 1987.
- Edward Tufte, *Envisioning Information*, on layering and "1 + 1 = 3".
- Nielsen Norman Group: "Response Times: The 3 Important Limits"; "Flat UI Elements Attract
  Less Attention and Cause Uncertainty"; "Hamburger Menus and Hidden Navigation Hurt UX Metrics".
