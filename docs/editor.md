# Writing a file

`edit` is a window you can type a file in. It is the smallest program on this
machine that is genuinely *useful* rather than demonstrative, and it exists for
two reasons: somebody should be able to write something down, and the
compositor's damage rectangles needed a program that would actually exercise
them.

## What it is lent

**One directory, read and write.** Nothing else.

That is worth dwelling on, because it is the only window here lent something it
may change. The picture viewer beside it is lent the whole filesystem
*read-only*, on the argument that a viewer which could write could delete a
photograph. An editor has to write, so the narrowing goes the other way: not a
narrower right over the whole disk, but a full right over one folder.

`DOCS`, made on demand by the compositor the first time somebody opens an
editor. It is the one directory on this machine that exists for the person
rather than for the system, so it is made when a person wants it.

What the editor cannot do: reach the network, start a program, see `SYSTEM`,
`PKG` or `PICTURES`, or write outside the folder it was handed. That last one is
not enforced by the editor — it is enforced by the kernel, because the editor
never sees a path. It asks its directory handle for a name. A program that does
not parse paths cannot parse them wrongly.

## Keys

The compositor does not forward Ctrl, so the commands are function keys.

| | |
| --- | --- |
| F2 | save |
| F3 | save as — the name field takes the keyboard until Enter or Escape |
| F5 | read the file again, losing anything unsaved |
| Escape | close; refuses once if there is unsaved work |
| Tab | four spaces |

Arrows, Home, End, Page Up and Page Down move the cursor. Enter splits a line,
Backspace at the start of one joins it to the line above.

## What it does not do

No selection, no clipboard, no undo, no search, no syntax colouring, no word
wrap. A line wider than the window scrolls sideways rather than folding. Each
of those is real work and none of it is pretended at.

The file is read whole, held whole, and written whole, because that is what the
filesystem underneath offers. A file above 256 KiB is **refused rather than
truncated**: a truncated file saved back is a file destroyed, and refusing to
open one is the only honest answer. The same for a file that is not UTF-8, and
for one with more than twenty thousand lines.

## Damage rectangles, which is the interesting part

The compositor used to repaint a client's whole window whenever the client said
it had drawn. For a wallpaper that was a full-screen composite four times a
second; for an editor it would be tens of thousands of pixels per keystroke.

A client can now say *which* part of its surface changed, and the editor is the
program that proves it works: typing a character declares one row and the status
line, and that is what gets repainted.

Getting it right needed a type. The first version tracked damage as
`Option<(usize, usize)>`, where `None` meant both "nothing has changed yet" and
"all of it changed" — so a keystroke after a scroll narrowed the repaint back to
one line and left the rest of the window stale. It is an enum now, with the
third state named, which makes that particular confusion impossible to write.

## And a bug worth remembering

The cursor was first placed by multiplying the column number by a character
width. That is wrong here: the font is eight pixels for Latin and sixteen for
Japanese, so the caret drifted along any line with kana in it. It measures the
text to the left of the cursor instead.

The same reasoning applies to the cursor's *column*, which counts characters
and not bytes — a byte offset would land inside a multi-byte character and the
next insertion there would panic.

## How it is tested

`scripts/test-editor.ps1`, and the shape of it matters more than the count.

It boots the machine, opens the launcher, names the editor, types two lines,
presses Escape once (which must be refused, because there is unsaved work),
saves, and then **starts the machine again**. The second boot opens the editor
and requires:

```
edit: NOTES.TXT read, 2 lines
```

A file that is right in memory and a file that is on the disk are different
claims. The first boot would pass just as well against a program that never
wrote anything; only the second one can tell.
