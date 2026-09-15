# A real sound card

```
[pci ]   00:04.0 8086:2415 class 04.01.00 -- multimedia device
[snd ] AC'97 at 00:04.0: 48000 Hz, 2 channels, 128 KiB of buffer, codec ready
[snd ] played the start-up chime through the sound card
[mon ] sound 3 notes played, 0 refused, 0 tones on the speaker (never used),
       3 through the card (31680 samples)
```

Until this existed the only thing on this machine that could make a noise was
the PC speaker. The speaker is one bit: it is on or it is off, and a tone is a
square wave made by a timer. It cannot play a recording, cannot mix, and cannot
be quiet at one volume rather than another.

The roadmap listed "a real audio card" as medium and described it exactly right
— *a DMA engine, a ring of buffers and a mixer*. That is what it turned out to
be.

## Two devices, not one

This is the part that is worth knowing before reading the driver.

The **controller** is on the PCI bus and owns the DMA engine. It reads a buffer
descriptor list — thirty-two entries, each a physical address and a sample count
— and walks it, handing samples to the link. That is the second I/O window,
`NABM`.

The **codec** is not on the PCI bus at all. It sits on the far side of a serial
link, and is reached by reading and writing the *first* window, `NAM`, which the
controller turns into link traffic. Volume, sample rate and reset live there.

Which means the order at bring-up is a protocol and not a preference: the link
comes out of reset first, and only then does the codec hear anything. A codec
whose link is still in reset answers every read with the same value and loses
every write, silently. That and a muted master volume — the field is
*attenuation*, so nought is loudest and a register left alone is not — are the
two things to check first when there is no sound.

## How it is known to work, without listening

Nothing in this repository can hear anything, so "it made a sound" has to be
established some other way.

The controller publishes where the engine has got to: which descriptor it is
reading, and how many samples are left in that buffer. Both change **only**
because the engine is reading memory. So `ac97::tone` watches them, and returns
false if they never moved:

```rust
if position != 0 || index != 0 {
    moved = true;
}
```

`sound.rs` falls back to the speaker when that happens, and says which one it
used. The whole claim rides on one line having two endings:

> played the start-up chime **through the sound card**
> played the start-up chime **on the speaker**

A driver that set every register and forgot the run bit prints the second. So
does one whose bus mastering was never enabled — and that one is worth naming,
because a device with bus mastering off enumerates perfectly, accepts every
register write, and simply never reads memory.

The test also checks the count: three tones and 31,680 samples, which is
90 + 90 + 150 milliseconds at 48 kHz in stereo, to the sample.

And it was made to fail, deliberately, by booting the same kernel with no card
attached:

```
FAIL never said '8086:2415'
FAIL the card played 0 tones and 0 samples
FAIL absent: no AC'97 card on this machine : it appeared
```

## Nothing is plugged into the host

The test machine gets `-audiodev none`. The card is entirely real to the
guest — it enumerates, its codec answers, its DMA engine reads guest memory —
and the samples go nowhere on the host. A test that opened the developer's
speakers on every boot is a test nobody runs twice.

## Why AC'97 and not Intel HD Audio

Because it is the simpler of the two by a long way. AC'97 is a descriptor list
and a codec on a serial link. HDA is a command ring, a response ring, a widget
graph to walk, and stream descriptors to allocate. The roadmap said "AC'97 or
Intel HD Audio"; this is the one that is a driver rather than a project.

## What it does not do

A square wave, because there is no floating point in this kernel and a sine
needs a table or an approximation — and a square wave at a known frequency
proves the engine runs exactly as well. What is different from the speaker is
not the waveform: it is that the samples are sixteen-bit, stereo, at a chosen
amplitude, through a codec at a set volume, and could as easily be a recording.

No mixing of two streams, no capture, no volume control after bring-up, no
variable sample rate. One caller at a time, because the second would overwrite
the first's buffer while the engine was reading it. Each of those is a real
feature and none of them is pretended at.
