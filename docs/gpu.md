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

So it is not the flush thread, and it is not the driver.

### What it is not, established since

Those two rows were measured when every write on this machine waited for the
host's disk, because the block driver refused `VIRTIO_BLK_F_FLUSH` --
[disk-barriers.md](disk-barriers.md) has that story. The obvious thought was
that the display migration had simply been measured on a machine that was
already thirty times slower than it should be, and that fixing the disk would
fix this too.

It did not. With the barrier in and a fresh format down to three seconds, a
machine given no firmware framebuffer **still does not finish one in four
hundred seconds**, twice running.

Nor is it the change in PCI layout. Removing the VGA moves every device up a
slot, so the disk lands at `00:01.0` on interrupt line 10 instead of `00:02.0`
on 11 -- two variables moving together, which is why the one fast headless log
could never settle anything. They can be separated: `-vga none -device VGA`
puts a display at the *end* of the bus, so the disk keeps slot one and line ten
and the firmware still gets a framebuffer.

| | fresh format |
| --- | --- |
| disk at `00:02.0`, line 11, framebuffer | 3039, 3264, 3076 ms |
| disk at `00:01.0`, line 10, framebuffer | **2210, 2384 ms** |
| disk at `00:01.0`, line 10, **no framebuffer** | did not finish in 400 s |

The middle row is the fastest configuration measured on this machine. So the
slot is innocent, the interrupt line is innocent, and what is left is the
framebuffer's absence itself -- which has no business touching a disk at all.

One more thing is known: a *minimal* headless machine is fine. Booted with no
GPU, no network card, no sound and no USB, on a disk that was already formatted,
`-vga none -display none` reaches the monitor thread and idles at 2.9 CPU
seconds per twenty -- an ordinary idle, not a spin. So it is not headlessness by
itself; it needs the full device set, or the fresh format, or both.

### Found: a driver that polls and never said so

Bisecting the devices off the bus found it. Headless formats in 3034 ms with the
GPU absent and would not finish in 400 seconds with it; nothing else on the bus
mattered.

The GPU driver polls its control queue and never wrote the flag that tells the
device not to interrupt. The line stayed asserted -- nothing read the GPU's
status register to acknowledge it -- and whichever driver shared that pin took
the interrupt over and over for a device it does not own. Invisible while a VGA
sat in the first slot, because that pushed the GPU onto a line nothing else
used. 883 interrupts from ring three without the GPU on the bus, **141204** with
it, 889 after the fix. Fixed in 2868a9e.

## The desktop is on it

`-vga none` in `qemu.ps1`. The firmware has no driver for a virtio GPU, so it
hands the bootloader no framebuffer at all, and the kernel's own driver becomes
the only thing that can put anything on the screen. There is no handover and no
moment when two drivers own one device.

```
[nexus-boot] no usable framebuffer; the kernel will run headless
[pci ] 9 devices told not to raise an interrupt; drivers that want one ask
[disp] the firmware gave no framebuffer; taking the GPU's 1280x800
[disp] 1280x800 display adopted
[mon ] gpu 92 commands answered, 44 rectangles flushed
[user] compositor: the session ended
```

### Damage, all the way down

Nothing flushes on a timer. Whoever draws says what changed:

- The **surface remembers**. `put_pixel` and `fill_rect` are the only two places
  in this kernel that store a pixel, so a bounding box kept there is complete
  without any drawing routine having to remember to say so -- and none of them
  can forget. `display::with` sends exactly that rectangle when it hands the
  surface back, after dropping the lock, because sending means waiting on a
  device and no other painter should wait for that.
- The **compositor** already computed a damage region for its own repaint, and
  now passes it down through a `DisplayFlush` system call. Forty-four rectangles
  in a session, against the 699 that twenty full screens a second produced.
- That call takes the **framebuffer handle**. Saying "I have drawn" needs the
  same authority as drawing, and on this machine drawing is done by having the
  display.

On a machine whose framebuffer came from the firmware every one of those calls
does nothing, because those pixels are already the screen. They are still made.
A compositor that asked first whether it was on a GPU would be one that has to
know, and that branch would be wrong on the machine nobody tested.

### What actually blocked it for a day

Not the flush thread, not the driver, not the disk, not the PCI slot, not the
interrupt line, and not the emulator. All six were measured and eliminated;
[disk-barriers.md](disk-barriers.md) has two of them.

It was that **a PCI interrupt pin is level-triggered and shared, and four slots
apart is the same line.** Removing the firmware's VGA moves every device up a
slot. The USB controller then lands on the disk's line, raises its pin, and
nothing acknowledges it -- the xHCI driver polls, and says so in a comment --
so the pin stays asserted and the *disk's* handler is called for ever, finding
each time that the interrupt is not its own:

```
disk 79513944, disk-returned 79513944   and the disk's own counter: 38124
```

Those two numbers looked like a contradiction and were the whole clue. The
handler ran seventy-nine million times and completed every time; the driver's
counter is incremented only when the device's status register says the interrupt
*was* the disk's. So the disk was answering "not mine" to every one of them.

Turning it off per driver was tried first and is the wrong shape -- it leaves
the default as "interrupt everybody", so the next device added here is a storm
waiting for the day some other device is removed and the slots shift. Four
separate bisections all pointed at "whichever device moves onto the disk's
line", which is not a device at all.

So the default is inverted. `pci::silence` tells every device on the bus not to
assert its pin, once, before any driver runs; the two drivers that genuinely
wait on an interrupt -- the disk and the network card -- ask for theirs back in
their own bring-up, next to the handler that will take it. **Taking an interrupt
is now an act rather than a default.**

```
before   disk 79513944
after    disk 42338      -- the same as the machine with a firmware display
```

### How it is checked

The old test looked for three bands of known colour, because the GPU had a
scanout nothing else drew into. It is the display now, so the bands are painted
over within a second, and a test that insisted on them would be insisting the
machine had no screen.

What it asks instead is harder to pass by accident. Black is what an
unconfigured display shows and so is any other single colour, so the question is
how many *different* colours are in the host's picture of this device, and
whether the top of it differs from the bottom:

```
ok   the host's picture of the GPU is 1280x800
ok   75 distinct colours in it, so the screen is on it
ok   top (11,20,40) and bottom (2,5,12) differ
```

Fifteen checks. And the desktop above it still works: the launcher starts
programs by name, and kanji conversion passes its eight.

## What is not here

No 3D, as above. No cursor plane, which virtio-gpu has as a second queue. No
multiple scanouts, though the device reports up to sixteen. No mode setting —
the display's size is taken as the host gives it. No interrupt: the control
queue is polled, which is right for a command that answers in microseconds and
wrong for anything that has to wait on a frame.
