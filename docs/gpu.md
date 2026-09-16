# A GPU driver

```
[pci ]   00:04.0 1af4:1050 class 03.80.00 -- display controller
[gpu ] virtio GPU at 00:04.0: 1280x800 scanout, 4000 KiB framebuffer, resource 1 attached
[gpu ] drew three bands into the scanout and the GPU took them: 1280x800, band 266
```

And from outside the machine, asking the host what that device is displaying:

```
scanout is 1280 x 800
row  133 centre pixel rgb=(255, 0, 0)
row  400 centre pixel rgb=(0, 255, 0)
row  667 centre pixel rgb=(0, 0, 255)
```

## The correction this file exists to make

For a long time the answer to "why is there no GPU driver" was that Phase 16 —
Vulkan, shaders, frame pacing — stands on one, and that a GPU driver is out of
reach. The second half of that was wrong, and it was wrong in a way worth
naming: **"GPU driver" was being used for two different things.**

One is a **display driver**. Find the device, negotiate with it, allocate a
framebuffer, tell the device that framebuffer is a scanout, hand it the
rectangles that have changed. That is an ordinary device driver, of the same
size as the USB stack already here, and there was never a good reason not to
have one.

The other is **3D**: a command stream in the device's own instruction set, a
shader compiler, a memory manager, and an implementation of OpenGL or Vulkan
above all of it. On real hardware that is a per-vendor instruction set and a
firmware blob. On virtio it is `virgl`, which needs a full GL implementation on
the guest side. Either is millions of lines.

The first is done. The second is still out of reach, and saying so remains
honest — but it is a different sentence from the one that was being said.

## What was actually needed

Most of this driver is not graphics. virtio-gpu has no *legacy* form — it was
defined after the 1.0 specification — so it is the first device here that needs
virtio's **modern transport**, and that is the bulk of the work.

Legacy virtio is a window of I/O ports at a fixed layout; the block and network
drivers on this machine use it, and their comments say why: it is a great deal
less code. Modern virtio puts nothing at a fixed place. The device's registers
are described by *capabilities* in its PCI configuration space — each says which
base address register a structure is in, at what offset, and how long — so the
first thing this driver needed was for `pci.rs` to be able to walk a capability
list at all, which it could not.

Then the status ladder, which is a protocol and not a set of flags:
`ACKNOWLEDGE`, then `DRIVER`, then features, then `FEATURES_OK` — and read it
back, because the device may refuse, which is the entire reason that step is
separate — then the queue, then `DRIVER_OK`.

`VIRTIO_F_VERSION_1` is the only feature claimed. Every other one changes the
layout or the meaning of something, and accepting a feature a driver does not
implement is agreeing to a protocol it does not speak.

## The graphics part, which is short

| `GET_DISPLAY_INFO` | how big the display is |
| `RESOURCE_CREATE_2D` | a surface of that size, in `B8G8R8X8` |
| `RESOURCE_ATTACH_BACKING` | guest memory behind it |
| `SET_SCANOUT` | make that surface what the display shows |
| `TRANSFER_TO_HOST_2D` | copy a rectangle of guest memory into the host's copy |
| `RESOURCE_FLUSH` | tell the host that copy changed |

The last two are both needed, and the failure modes differ: a driver that sends
only the transfer writes into a buffer nobody looks at, and one that sends only
the flush shows the previous frame again.

The format is `B8G8R8X8`, which is what the framebuffer on this machine is
already in, so nothing has to be swizzled on the way.

## Why the evidence is what it is

Nothing in this repository can look at a screen, so "the GPU is displaying it"
has to be established some other way — and the usual trick of checking the log
does not work here, because the log is written by the driver whose behaviour is
in question.

So there are three separate claims, each checked differently.

**The device answered.** `GET_DISPLAY_INFO` returns 1280x800, and that number
came from the host. A driver that sent nothing and reported success would have
to invent it and would get it wrong.

**The device accepted.** Every command is answered with a type code the driver
did not write. `VIRTIO_GPU_RESP_OK_NODATA` coming back from a scanout that was
never set is not something this code can produce on its own.

**The pixels arrived.** The kernel fills the scanout with three bands and the
test asks QEMU for a picture of *that display device* — named, because without
the name QEMU captures whichever display it counts as first, which on this
machine is the firmware's, and the test would be looking at the desktop. Black
is what an unconfigured display shows too, so a blank capture would prove
nothing; a red band a third of the way down proves the whole path.

And the test was made to fail, by booting the same kernel with the device
removed: six of eleven checks went red, including "absent: no virtio GPU on this
machine : it appeared".

## Where it sits

Beside the firmware's display, not instead of it. The bootloader is handed a
framebuffer by the firmware's graphics protocol, and everything drawn on this
machine has gone into it since the first boot. `-vga none` would mean the
firmware driving this same device *and* the kernel driving it, which is two
drivers on one device.

So the compositor still draws into the firmware's framebuffer, and the GPU has a
scanout of its own.

## Moving the desktop onto it: tried, measured, not shipped

The obvious next step is for the GPU to *be* the display. It was built and it
works, and it is not in the tree, because of what it costs.

The shape of it is tidier than expected. This firmware has no driver for a
virtio GPU, so a machine given one and `-vga none` is handed **no framebuffer at
all** — `no usable framebuffer` from the bootloader. There is then no handover
and no moment when the firmware and the kernel are both driving one device: the
kernel's own driver is simply the only thing that can put anything on screen.
`display::init` takes the GPU's scanout described as an ordinary
`FramebufferInfo`, and nothing above it — not the boot display, not the
compositor — ever learns which it got.

It boots. The desktop appears. And the machine becomes an order of magnitude
slower:

| | fresh NexusFS format |
| --- | --- |
| no GPU | 33.0 s |
| GPU present, firmware display | 33.4 s |
| GPU as the display | did not finish in 300 s |

The middle row is what makes the measurement worth keeping. The GPU's
*presence* costs nothing measurable — the same driver, the same bring-up, the
same 4 MiB scanout allocated and attached. Only adopting it as the display is
expensive, and it is expensive during a disk format, which happens long before
anything flushes a single rectangle.

So it is not the flush thread, and it is not the driver. What it is has not been
found, and that is the whole reason this is a section in a document rather than
a commit: shipping it would trade a display that works for one that works and
makes the machine unusable, in exchange for a picture that looks identical.

The flush thread that went with it was real — twenty times a second, four
megabytes a frame, `1402 commands answered, 699 rectangles flushed` in a boot.
It is the wrong design anyway: whoever draws should say what changed, which is
exactly the damage rectangle the compositor already computes for its clients.
Routing that down to the driver is what the migration actually needs, and it
would cut the traffic by whatever fraction of the screen is still.

## What is not here

No 3D, as above. No cursor plane, which virtio-gpu has as a second queue. No
multiple scanouts, though the device reports up to sixteen. No mode setting —
the display's size is taken as the host gives it. No interrupt: the control
queue is polled, which is right for a command that answers in microseconds and
wrong for anything that has to wait on a frame.
