# Reading and writing a USB drive

```
[pci ]   00:04.0 1b36:000d class 0c.03.30 -- USB controller
[usb ] xHCI at 0xc000000000: 64 device slots, 8 ports, 32-byte contexts
[usb ] running: 0 scratchpad pages for the controller's own use
[usb ] port 1 has a device on it at SuperSpeed, enabled
[usb ] port 1: 46f4:0001, class 08.06.50 in slot 1
[usb ] QEMU QEMU HARDDISK: 2048 blocks of 512 bytes, 1 MiB
[usb ] read block 5 and it holds what the image was built with
[usb ] wrote block 2047 and read it back: the drive can be written to
```

## Four layers, because it really is four

The roadmap has said since long before any of this existed that USB storage is
"xHCI, then USB enumeration, then mass storage, then SCSI — four layers, each of
which is a driver on its own." That was accurate, and it is why the disk this
machine boots from is virtio: virtio is one layer and a ring of descriptors.

| `xhci.rs` | the controller: reset, ports, slots, commands, events |
| `xhci_rings.rs` | TRBs and the cycle bit, which is the whole synchronisation protocol |
| `usb.rs` | what any USB device is: descriptors, endpoints, configuration |
| `usb_storage.rs` | bulk-only transport, and the SCSI commands inside it |

Each knows nothing about the one above it. `xhci.rs` has never heard of a disk;
`usb_storage.rs` has never heard of a TRB beyond asking for a transfer.

## The cycle bit

A ring has no head pointer. The producer writes each entry with the current
cycle value and flips that value every time it wraps; the consumer reads until
it meets an entry whose cycle is not what it expects. One bit, and it is the
entire protocol in both directions.

Getting it wrong does not fail loudly, which is what makes it worth writing
down. A driver that forgot to flip its own cycle on a wrap quietly stops being
read. One that forgot to flip the *expected* cycle on the event ring reads the
same events for ever. Both look exactly like a controller that has stopped
answering, so the flip happens in exactly one place per ring.

## The bug this cost

The first `GET_DESCRIPTOR` came back reporting that nothing had moved. The
completion code said success; the residue said 16777216 bytes were left over,
out of a requested eighteen.

16777216 is `1 << 24`. The residue field is twenty-four bits — 0 to 23 — and
the *completion code* sits in bits 24 to 31. My mask was one bit too wide, so
it was picking up the low bit of the completion code, and completion code 1 is
success. The transfer had worked perfectly every time.

It is written down here because of how it presented: it looked like a device
returning no data, which sends you looking at setup packets and endpoint
contexts and transfer types. It was arithmetic.

## What is checked, and how

The image QEMU plugs in is built by `scripts/make-usb.ps1`, and every sector of
it says its own number:

```
NEXUS USB SECTOR 000005 NEXUS USB SECTOR 000005 ...
```

That shape is the test. A driver that returns the first sector for every
request, or is out by one, or reads the right number of bytes from the wrong
place, passes against a disk of zeros and fails against this one. So the kernel
reads block **five**, not block zero, and checks the text says five.

The write half is checked by writing the last block, then reading a **different**
block, then reading the written one back. The middle step is the one that
matters: without it, a read-back would pass against a driver whose write and
read both did nothing and left the buffer alone.

And the bytes really leave the machine — after a run, the host's own image file
holds them:

```
000ffe00: 4e45 5855 5320 5752 4f54 4520 5448 4953  NEXUS WROTE THIS
000ffe10: 204f 5645 5220 5553 422e 2e2e 2e2e 2e2e   OVER USB.......
```

## A filesystem, on a second stick

```
[usb ] port 2: 46f4:0001, class 08.06.50 in slot 2
[usb ] QEMU QEMU HARDDISK: 131072 blocks of 512 bytes, 64 MiB
[usb ] drive 1 mounted: "NEXUSOS", 5 entries in the root
[usb ]   HELLO.TXT is 35 bytes and begins "NexusOS reads its own filesystem."
```

`fs/fat32.rs` used to call the virtio driver by name, which made "mount the
stick" a rewrite rather than an argument. It takes a `Source` now, and neither
it nor `fs/gpt.rs` knows or cares which kind of disk the sectors came from --
a FAT32 filesystem is a FAT32 filesystem whether they arrive over one layer or
four.

Two sticks rather than one, on purpose. They prove different things: the raw
one proves blocks reach the right place, the formatted one proves a filesystem
can be read. And two devices means the driver has to enumerate more than one,
which is a thing it could quietly have failed to do.

The directory listing is not the whole claim either -- a listing proves the
*directory's* cluster chain was followed and says nothing about a file's, which
is a different chain. So it reads a file as well.

## What is not here

**No interrupts.** Every wait is a poll with a deadline. This runs at start-up
and during explicit reads rather than in a hot path, and a polled driver that
works is worth more than an interrupt-driven one that is half written. Every
poll is bounded, so a controller that stops answering costs a timeout and a line
in the log rather than a machine that stops.

**No hubs.** A device plugged into a hub plugged into the machine needs the
route string in its slot context, and the hub itself is another device to
enumerate. Only root ports work.

**One transfer at a time, one page at a time.** `MAX_TRANSFER` is 4096 bytes —
eight blocks. A larger transfer needs either a bigger buffer or a scatter list.

**No hot-plug.** Ports are surveyed once, at start-up. A stick pushed in
afterwards is not noticed; the port status change events that would say so are
read off the event ring and discarded.

**Not yet reachable from a program.** The kernel reads and writes the drive's
blocks and mounts a filesystem off it; nothing *above* the kernel can, because
there is no handle to hand a program. That is the next piece and it is smaller
than any of the four layers below it.

**Older controllers are named, not driven.** EHCI and UHCI are a different data
structure, not a simpler version of this one, so a machine with one is told it
has a USB controller this does not speak.
