# Pictures

The machine can show a picture. What that took, from the bottom:

```
shared/nexus-inflate    DEFLATE and zlib          RFC 1951, RFC 1950
shared/nexus-image      PNG and BMP               into 0x00RRGGBB pixels
user/nexus-view         a window                  chooses, fits, draws
```

## Why it was written rather than taken

This tree has no third-party dependencies, and the reason is not purity. Every
crate on this machine has to build for `x86_64-nexus-user`, a custom target with
no operating system under it, no floating point in the kernel, and `no_std`
everywhere. A decompressor is four hundred lines and a fixed algorithm; the work
of adapting an existing one would not have been much smaller than writing it,
and the result would have been code nobody here had read.

That calculus does not hold everywhere. It holds for DEFLATE. It does not hold
for H.264, and the roadmap says so.

## What is decoded

**PNG**: greyscale, truecolour, palettised, and both greyscale and truecolour
with alpha, at one, two, four, eight and sixteen bits per sample, with all five
row filters. Interlaced images are refused by name — Adam7 is a second pass over
everything here, for a feature almost nothing writes any more.

**BMP**: uncompressed twenty-four and thirty-two bit, which is what a screenshot
is. Palettised and run-length encoded bitmaps are refused by name rather than
guessed at.

**JPEG** is recognised and refused by name. It is the next one worth doing.

The kind is read from the first bytes and never from the name. A file called
`.png` that is a bitmap is a file somebody renamed, and the bytes are the only
thing that knows.

## Alpha, and why the caller chooses the background

The surface a program draws into has no alpha channel: the compositor
composites windows, not pixels within one. So transparency is flattened at
decode time, onto a colour the caller passes in. The viewer passes its own
window background, so a transparent picture sits on the window rather than on
black — and the same file on a different background is a different picture,
which is the point of taking one.

## Bounds, because every input came from somewhere else

* Dimensions are checked before anything is allocated, and their product is
  computed with `checked_mul`. A header claiming four billion by four billion is
  refused rather than believed.
* The caller says how many pixels it will hold. The viewer allows one megapixel,
  which is what its heap allows: four bytes a pixel for the picture, about the
  same again for the rows the decoder unfilters, and the compressed file on top.
* The decompressor's output is capped by its caller, every table index is
  checked, and a back-reference pointing before the start of the output is an
  error rather than a wrap.
* **A PNG without its `IEND` chunk is truncated**, even when every pixel
  arrived. Other decoders are lenient here; this one is not, because the
  alternative is a half-downloaded picture that decodes and looks fine.

## The heap ceiling, which is a real constraint

`ipc::MAX_MEMORY_OBJECT` is sixteen mebibytes: the kernel will not make a memory
object larger, because the size comes from a process and an unbounded one would
let a program ask the kernel to set aside all of memory one page at a time.

A program's heap is one such object. The viewer asked for forty-eight
mebibytes, was refused, and died before it could draw a word — which is how that
number came to be measured rather than guessed. It asks for twelve now, and says
plainly what happened if even that fails.

## Where the picture comes from

`scripts/make-picture.ps1` draws the Nexus mark with `System.Drawing` on the
build machine and saves it as a PNG. The kernel copies it onto the store at
boot, the same way it copies packages: anything ending `.PNG` or `.BMP` in the
image's program directory goes to `PICTURES/`.

It is written by a reference encoder on purpose. A decoder checked only against
pictures written by its own encoder is a decoder that agrees with itself.

## Testing it

Two layers, because they can check different things.

**On the build machine**, `nexus-inflate` has nine tests and `nexus-image` has
nineteen, against fixtures produced by Python's `zlib` and by writing PNG chunks
to the specification. That is where malformed input can actually be produced: a
truncated file at every possible cut, a flipped byte, a header claiming an
impossible size, a surrogate escape with no pair. The guest has no compressor,
so it could not make any of those.

**In QEMU**, `scripts/test-view.ps1` presses the sixth button on the strip and
waits for the viewer to say it decoded the picture the image ships. That is the
part the host tests cannot reach: a read-only directory handle, a real file off
a real filesystem, and thirteen kilobytes of dynamic Huffman blocks decoded
inside the machine.

## What the viewer is given

One handle: the filesystem, read and transfer, and **not write**. A picture
viewer that could write is a picture viewer that can delete a photograph.
