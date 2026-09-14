# The boot logo

Drawn from primitives rather than loaded from a file.

At the moment it appears there is no filesystem: the disk has not been found, the
block cache does not exist, and the heap is the only thing that works. A logo
that needed a file would be a logo that could only appear after the machine was
already up, which is the one moment it is not wanted.

## What it draws

A nexus: twelve lines converging on a node, brighter towards the middle, with
the name under them and the version under that. Twelve fixed directions from a
table, because there is no floating point in the kernel and a table of twelve is
smaller than the arithmetic that would avoid it.

Sized against the smaller side of the display, so it is the same shape on a wide
screen and a tall one.

## The line underneath

It moves with bring-up, not with time. Six steps — the scheduler, the self-test,
the clock, the keyboard, the network, and the first program that is not the
kernel's — and the bar is filled to whichever it has reached.

A progress bar driven by a timer is a decoration. This one is the machine saying
what it has got through, so a machine that is slow because its disk is slow shows
a bar that pauses in the same place every time, which is a thing somebody
debugging can use.

## How long it is up

Exactly as long as bring-up takes. It is painted the moment the framebuffer is
adopted and replaced by the diagnostics panel when the display thread makes its
first pass. There is no timer holding it and no minimum duration: a machine that
starts quickly shows it briefly, which is the honest thing for it to do.
