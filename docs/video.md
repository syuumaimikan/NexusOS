# Playing video

NexusOS plays Motion-JPEG. Open the **Pictures** window from the strip; a
recording in `PICTURES/` plays as soon as it is selected, and **Space** stops
and starts it.

The machine ships with one: `PICTURES/NEXUS.AVI`, twenty-four frames of the
boot mark turning, so the player can be used before anybody has put a file on
the disk.

## What Motion-JPEG is, and what it is not

Every frame is a complete JPEG. There is no motion estimation, no prediction
between frames, no B-frames and no rate control: twenty frames a second of a
talking head costs the same as twenty photographs of a talking head.

That is why nobody streams films this way and why every webcam, scanner and
security camera does. And it is why it is here: a real codec is tens of
thousands of lines and a patent history, while this is a container reader on
top of the JPEG decoder that already existed. It says something true about the
machine — it plays video — without pretending to something it cannot do.

**H.264 and VP9 are not here and are not close.** The roadmap says so.

## The file is never read

This is the part that decides the design.

The kernel will not make a memory object larger than sixteen mebibytes, because
the size comes from a process and an unbounded one would let a program ask the
kernel to set aside all of memory. A player that read a recording into memory
would therefore be a player with a running time compiled into it — about two
minutes, at which point it would stop working with no way for a person to tell
why.

So the file stays open and only its **table of contents** is held:

| | |
| --- | --- |
| Held per frame | 16 bytes — where it is, how long it is |
| Read to build the index | 8 bytes per frame, once, at open |
| Held while playing | one compressed frame, one decoded frame |
| Held of the file itself | nothing |

A minute of video is six hundred reads of eight bytes — about five kilobytes of
I/O to learn where eight megabytes of frames are. Playing reads one frame's
bytes, decodes them, draws them, and lets both go.

## The container, and the index that is ignored

AVI is RIFF: four-byte tags, little-endian lengths, every chunk padded to an
even byte.

```
RIFF "AVI "
  LIST "hdrl"
    avih                  frame interval, size, count
    LIST "strl"
      strh                'vids', 'MJPG', rate/scale
      strf                BITMAPINFOHEADER, biCompression 'MJPG'
  LIST "movi"
    00dc ...              one complete JPEG per chunk
  idx1                    an index, deliberately ignored
```

`idx1` gives every frame's offset — and a reader has to guess whether those
offsets are relative to the `movi` list or absolute in the file, because
writers disagree. A reader that guesses wrong reads rubbish. Files written for
streaming have no index at all.

Walking the chunks needs neither guess and works on both. The cost is that a
frame can only be reached by passing the ones before it, which is exactly what
playing forwards does anyway.

The pad byte after an odd-length chunk is not counted in that chunk's length. A
reader that forgets it lands one byte early from the first odd-length frame
onwards and finds nothing after it. There is a test for exactly that.

## The clock

`dwMicroSecPerFrame` from the file's own header. The next frame's deadline is
**the last deadline plus one interval**, not "now plus one interval" — the
difference is drift. A decode taking sixty milliseconds at ten frames a second
would otherwise make every frame sixty milliseconds later than the last, and a
minute of recording would take two minutes to play.

If it falls more than a second behind, the schedule restarts from now. Without
that, a machine which cannot keep up would decode frames as fast as it can for
ever, chasing a schedule it will never meet.

## How fast it actually is

Reported, not assumed. The player logs what it managed each time round:

```
view: played 24 frames, decoding at 66.6 a second, asked for 12.0
```

Fifty to sixty-six frames a second in QEMU, for a recording that asks for
twelve — four or five times the headroom needed. `scripts/test-view.ps1`
requires that line to appear and prints the figure, so a change that makes
decoding slower shows up as a number going the wrong way rather than as
nothing.

It is not asserted against a threshold. QEMU on a loaded build machine is not a
benchmark, and a threshold would fail for reasons that have nothing to do with
this code.

## Testing it against something that is not us

`shared/nexus-image/fixtures/` holds files from **ffmpeg**, not from this
project:

* `tiny.jpg`, `tiny422.jpg`, `tiny444.jpg` — one frame at each chroma sampling
* `tiny*.rgb` — ffmpeg's own decode of each
* `clip.avi` — ten frames of a test pattern

Comparing against those is the one test that can catch a decoder which is
*consistently* wrong — the failure a decoder checked against its own encoder
cannot see, because both halves share the mistake. Measured: mean error under
one level out of 255, worst error three or four, on all three samplings. The
bounds in the tests are about double that, which leaves room for an ffmpeg that
rounds differently and none for a decoder that is actually wrong.

The clip that *ships* is written by `scripts/make-video.ps1`, because ffmpeg is
not on every build machine and `build.ps1` has to work on all of them. Its
frames are System.Drawing JPEGs; only the RIFF wrapper is this project's own
idea of the format, and ffprobe reads the result correctly.

The generator burns the frame number into each frame, which turned out to be
the most useful thing about it: a screenshot shows `2 / 24` in the picture and
`paused, frame 2 of 24` in the status line, and those two agreeing is proof the
container offsets are right that no amount of "it decoded" could give.

## Making a file this will play

```
ffmpeg -i whatever.mp4 -c:v mjpeg -q:v 5 -pix_fmt yuvj420p out.avi
```

Then put `out.avi` in `PICTURES/`. Any of `yuvj420p`, `yuvj422p` or `yuvj444p`
works; `-q:v` is 2 (best) to 31.

Motion-JPEG files are large — expect roughly ten times an H.264 file of the
same quality. That is the format, not this decoder.

## What is refused, and by name

| | |
| --- | --- |
| Progressive JPEG frames | Refused. The format sends the same coefficients over several passes, which cannot be decoded block by block. |
| Any other codec in an AVI | Refused by name, not decoded as noise. |
| Sound | There is none. The audio stream in a file is skipped; the speaker is one bit. |
| Seeking | Not there. Playing is forwards, and it loops. |
| Frames larger than 8 MiB | Refused. One frame has to fit in memory to be decoded. |
| More than 12,000 frames | Refused. Twenty minutes at ten a second, which is more than this machine has storage for. |

A recording cut off mid-copy is **not** refused: the frames found before the
cut all play, because most of a recording is more use than an error message.
