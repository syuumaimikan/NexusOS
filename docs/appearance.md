# What this machine looks like

Four values in the settings file, and a program that draws the background.

```
look.style  = gradient | plain | stars | rings | grid
look.top    = 0b1428
look.bottom = 040814
look.accent = 388be8
```

Change one with `set look.style stars` in the terminal, and the machine follows
within two seconds. Nothing has to be restarted and nothing has to be told.

## Why the wallpaper is a program

Because the alternative is the compositor deciding what a desktop looks like,
and the compositor is the one program whose decisions nothing else can replace.

A background is *taste* — a colour, a pattern, one day a picture. Taste in the
program that owns the framebuffer is the same mistake as policy in the kernel,
one layer up. So `user/nexus-wall` draws into a surface exactly as a window
does, and the compositor composites it first without knowing what is in it.

What that buys is concrete. Replacing the wallpaper is replacing one program. A
wallpaper that crashes is a black rectangle, not a machine with no display. And
a wallpaper that wants to be a video player one day needs nothing from the
compositor that it does not already have.

## Why the compositor is *told* the accent

It draws title bars, focus rings and resize grips, so it has to know the colour
— but it must not read the settings file, for the same reason it must not draw
the background. So the kernel reads one number out of the settings and hands it
over in the same message that hands over the framebuffer, next to the bit that
says whether the machine has been set up.

One number, not a copy of the settings. What the setting *means* stays with
whoever shows it.

## Still and moving

`plain`, `gradient` and `grid` do not change, so the program that draws them
blocks and costs nothing; `stars` and `rings` ask to be woken eight times a
second. The pattern decides, because the pattern is the only thing that knows
whether anything would change — a still background redrawing on a clock would be
a machine spending a frame a second on a picture nobody is watching change.

The star field keeps nothing between frames: each point's position, speed and
brightness come from a hash of its number, so the pattern is decided by one
number and is the same on every boot. The rings are the midpoint circle
algorithm, integers all the way down, for the same reason the boot logo uses a
table of directions — there is no floating point here and none is wanted.

## What it cannot do

Pictures and video. A picture needs a decoder for whatever format it is in; a
video needs one that runs thirty times a second and a pipeline to keep it fed.
Neither is here. The patterns are drawn rather than loaded, which is why they
cost a few hundred lines instead of a few hundred thousand — and why the honest
version of "Wallpaper Engine" on this machine is a program that *computes* its
background rather than one that plays a file.

## Adding a style

One arm of a `match` in `shared/nexus-look` for the name, and one in the
wallpaper for the drawing. The name is what somebody types and what
`nexus_look::Style::ALL` lists, so `look` in the terminal shows it without being
told.

A style that moves says so with `moves()`, and that is the whole of its contract
with the rest of the system: the program drawing it asks to be woken only if the
answer is yes.
