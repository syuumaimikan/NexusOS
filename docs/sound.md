# Making a noise

There is no system call that makes a sound. A program reaches the speaker
through a channel, exactly as it reaches the network and the filesystem, and
holding an end of that channel is the whole of the authority.

That matters more for sound than it looks. The speaker is the one output on this
machine that cannot be ignored by looking elsewhere, so "which programs can make
a noise" is a question worth being able to answer by looking at what they were
given.

## What the hardware is

Channel two of the same interval timer the kernel used for its clock, wired to a
speaker instead of to an interrupt. Two port writes start a tone and one stops
it, and between those it makes the sound on its own — which is what makes it the
only sound device here that costs nothing while it is playing.

One bit. A square wave at one frequency: no volume, no envelope, no second voice,
no sampled sound. Everything above a beep needs a real audio card, and that is a
DMA engine, a ring of buffers and a mixer — a driver, not a port write. It is not
pretended at.

Using channel two cannot disturb the clock, because it is not the clock: the tick
moved to the local APIC timer during bring-up, and channel two was never used.
The two bits touched in port `0x61` are the speaker's gate; the other six belong
to other things and are read and written back unchanged.

## Why it is a thread

A tone lasts. Playing one means starting it, waiting, and stopping it, and the
waiting must not happen on the caller's thread — a program that asked for a
half-second chime would be a window that stopped repainting for half a second.
So a request returns at once and a thread of the kernel's own does the waiting.

That also makes the queue meaningful. Two programs asking at the same moment
cannot both have the speaker, so the second waits for the first rather than
cutting it off, and a program that asked for a sequence gets a sequence.

## The chime

Three rising notes when the machine has finished starting, played before
anything can ask for anything else — so the chime is the chime rather than
whatever a program queued during bring-up.

It is there because the first thing a person wants from a new operating system is
for it to go *ding* when it starts, and because a machine that cannot make a
sound at all cannot tell you anything when you are not looking at it.
