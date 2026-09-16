# The barrier the journal never had

```
[blk ] the disk takes a flush, so the host may cache what is written
[mon ] disk 78222 requests, 78222 finished on the poll, 0 slept, 161 flushes
```

A fresh eight-gigabyte NexusFS format took thirty-three seconds. It takes three.
The change is one line of feature negotiation and three calls, and it makes the
filesystem *safer* rather than trading safety for speed, which is unusual enough
to be worth the page.

## What was wrong

`virtio_blk.rs` wrote zero to the device's feature register. The comment said
why, and the reasoning was good: every optional feature changes the layout or
the meaning of something, and a driver that accepts one it has not implemented
is agreeing to a protocol it does not speak.

But refusing `VIRTIO_BLK_F_FLUSH` is not free, and the cost is not paid where
anybody was looking. **A host cannot give a write-back cache to a guest that
cannot ask for a barrier.** There would be no way for the guest to say "this
much must survive", so the host has to treat every write as if it were that
point. QEMU does exactly this: it runs the drive write-through, and every
four-kilobyte write waits for a real disk.

## And the journal was relying on it

The journal writes four things in a fixed order — the blocks, the descriptor
that commits them, the blocks again in their real places, then the erasure of
the descriptor. That order *is* the protection. Nothing enforced it.

It worked because the device was never given a cache to reorder within. That is
a guarantee nobody asked for, that cost thirty seconds of every format, and that
would have disappeared silently the first time this driver claimed any feature
at all — including a feature added for an unrelated reason years later, by
somebody with no idea the journal was leaning on the absence of a cache.

So there are now three barriers, each at a place where the ordering carries
weight:

| after the blocks, before the descriptor | a descriptor that lands first describes blocks that do not exist |
| after the descriptor, before `apply` | a stop mid-apply must leave a record saying what the blocks should become |
| after `apply`, before the erase | "this operation is finished" must not be said about one still in progress |

## Measured

Every figure below is a fresh image made by the same script, and the spread
column is why there are three runs rather than one.

| | fresh format | spread | durability |
| --- | --- | --- | --- |
| before | 32169, 32807, 34242 ms | 1.9x | accidental write-through |
| `cache=unsafe` | 2294, 2724 ms | 1.2x | **none** |
| after | 3039, 3264, 3076 ms | 1.1x | explicit barriers |

Within a fifth of what a host told to ignore flushes can manage, with durability
that is now asked for rather than inherited.

Mounting an existing filesystem is unchanged: **1248 and 1258 ms**, a spread of
1.00. That needed its own measurement, because `Measure-Format` makes a new
image every time and a mount needs one that is already there.

And the journal's destructive checks pass with the barriers in, which is the
claim that matters most:

```
[test] journal verified: a commit without its writes was replayed by the next
       mount, a second mount found nothing left to do, and an abandoned
       transaction left no trace
```

## Four wrong answers, in the order they were eliminated

This took a day, and most of it was spent on things that were not the cause.
They are listed because each was believed, and because the measurement that
killed each one is cheap and reusable — `scripts/test-disk-speed.ps1` runs all
of them.

1. **A regression over time.** The story began as "8516 ms at one commit, 33
   seconds ever since". It was wrong: there was a 709 ms log, later than all the
   slow ones, that had not been looked for. GPT-6 Astra found it and the framing
   collapsed.
2. **A shared interrupt line.** The fast run had the disk on a line of its own.
   Removing the network card unshared the line and the format got *slower*.
3. **The display.** The fast run also had no VGA device. Removing it did not
   finish a format in four hundred seconds.
4. **Software emulation.** Nothing here has ever passed `-accel`, so this had
   all been running on TCG. Hardware acceleration changed nothing.

The one that mattered came fifth, and only after the first measurement of all:
**four identical runs span 17610 to 34242 ms.** Before that number existed, two
figures from inside that spread had already been placed side by side and called
a result.

## What a flush costs

161 of them in a boot, at roughly a millisecond each — about a sixth of a
second, against the thirty seconds they save. A journal commit is three, and a
commit of one block is therefore more expensive than it was; a commit of many is
enormously cheaper. Nothing here batches commits, and if the small-transaction
case ever matters, that is where to look.

## The counters that were already there

`REQUESTS`, `POLL_COMPLETIONS` and `WAIT_ATTEMPTS` were being incremented and
printed nowhere. They now are, and the first line they produced was this:

```
78222 requests, 78222 finished on the poll, 0 slept
```

Not one request in seventy-eight thousand used the blocking path. The interrupt
handler's wake, the wait queue and the whole `BLOCKING` apparatus are dead
weight in practice — the spin before the block always wins. The log had been
saying `requests block`, which means blocking is *enabled*, not that anything
blocked, and reading it the other way cost most of a day.
