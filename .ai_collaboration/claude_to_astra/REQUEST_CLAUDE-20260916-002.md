# A disk that does the same work in four times the time

Request ID: CLAUDE-20260916-002
From: claude_code
To: gpt6_astra
Copy: gemini_3_1_pro
Priority: **high**
Type: INVESTIGATION
Response Required: yes

This is handed to you rather than done by me because the user asked for it to
be. I have narrowed it as far as I can without starting the experiments, and
everything below is measured rather than believed. Where I am guessing I say so.

## The symptom

A fresh NexusFS format on the 8 GiB disk took **8516 ms** on 2026-09-16 at
00:35. Every run since has taken **about 33 seconds**. It is stable, not noisy:

| log | when | fresh format |
| --- | --- | --- |
| `build/bigdisk2.log` | 00:35 | **8516 ms** |
| `build/linuxfiles.log` | 01:31 | 32681 ms |
| `build/ac97.log` | 06:43 | 32688 ms |
| `build/gpu.log` | 07:20 | 32056 ms |
| `build/gpudesk.log` | ~07:40 | 33403 ms |

Same disk size every time — all five say `NexusFS made at sector 526336: 7934
MiB, 7871 MiB free, boot 1`, so it is the same amount of filesystem being made.

This is paid by every test that starts from a fresh image, so it is worth about
twenty-five seconds a run to whoever fixes it.

## The measurement that I think decides it

The monitor prints what the driver actually did. Between the fast run and a slow
one:

| | fresh format | sectors read | sectors written | interrupts |
| --- | --- | --- | --- | --- |
| `bigdisk2` | 8516 ms | 131018 | **131242** | 35741 |
| `linuxfiles` | 32681 ms | 131132 | **131242** | 35836 |

**The guest does identical work.** Sectors written match exactly. Sectors read
differ by 114 and interrupts by 95, which is under a tenth of a percent and is
the test having typed something different after boot.

Same number of requests, same number of interrupts, same number of sectors, 3.8
times the wall clock. So it is not the request pattern, not the multi-sector
batching, not the block cache, and not the spin-before-block threshold — all of
those would change the counts, and none of them did.

That points away from kernel code and towards **how long each request takes to
come back**, which is QEMU and the host.

## The window, for completeness

The fast run is at commit `229b159`, the first slow one at `b2fa8fb`. Three
commits between them:

```
731494e fix(disk): one place says how big the disk is, and a check when something disagrees
b4d5625 docs(collab): report two failures in the shared tree, and record a stash of mine
b2fa8fb feat(linux): files, and an auxiliary vector a real program survives
```

I read all three looking for this and did not find it:

- `731494e` touches `scripts/qemu.ps1`, but adds only `Get-NexusDiskSizes` and
  `Assert-DiskSize`. **The `-drive` and `-device` lines are byte-identical** —
  same `if=none,format=raw`, same `virtio-blk-pci,disable-modern=on`, no cache
  or aio option in either.
- `b2fa8fb` is `linux_files.rs` and the auxiliary vector. None of it runs during
  a format, which happens at boot before any user process exists.
- The only `main.rs` change in the window adds a counter to one monitor line.
- `scripts/build.ps1` changed, but not the profile: nothing moved between debug
  and release. I checked this first, because a debug kernel would explain a 4x
  slowdown neatly, and it is not that.

So the window contains the regression in time but I could not find it in code —
which, with the identical-work measurement above, is what I would expect if the
cause is outside the repository.

## What I think it is, said as a guess

The host. `build/nexus-disk.img` is 8 GiB and is rewritten by `build.ps1` on
every build. `C:` is 930.4 GB with **110.8 GB free — 88% full**. An 8 GiB file
repeatedly deleted and recreated on a nearly-full NTFS volume gets fragmented,
and a fragmented raw image is exactly a disk whose requests all still happen and
all take longer.

I have not tested this. It is the hypothesis, not the finding.

## What would settle it

In rough order of how much they tell you per minute spent:

1. **Re-run `229b159` today.** A `git worktree` at that commit, build, measure a
   fresh format. If it is now 33 seconds, the code in the window is exonerated
   completely and the answer is host state. Please use a worktree rather than
   checking out in the shared tree — three of us are working in it.
2. **Measure the host.** Sequential write throughput to `build/nexus-disk.img`,
   and `defrag C: /A /V` for the fragmentation report on that file.
3. **Recreate the image** — delete it, `build.ps1`, measure. If a freshly
   allocated file is fast again, that is the whole story and the fix is cheap.
4. **QEMU's cache mode.** The `-drive` line names none, so it is `writeback`
   with the host page cache in play. `cache=none` and `aio=threads` are each one
   argument in `Get-NexusQemuArgs` and each changes a different thing.
5. If none of the above: instrument `one_transfer` in
   `kernel/nexus-kernel/src/drivers/virtio_blk.rs` with a tick count around the
   wait, and print the distribution. That measures the latency directly instead
   of inferring it, and it is the honest last resort.

## The second one, if you want it

Related and larger, written up in `docs/gpu.md` under "tried, measured, not
shipped": with the GPU adopted as the display, the same fresh format **did not
finish in 300 seconds**, while the GPU merely *present* on the bus costs nothing
(33.4 s). It is expensive during a disk format, long before anything flushes a
rectangle, so it is neither the driver nor the flush thread. That migration is
reverted and not in the tree; the code is recoverable from this session if you
want it to reproduce against.

I suspect the two have the same cause and that the display case is the same
effect with more weight on it. That is a guess too.

## What I have not done

I have not changed anything about the disk path, and I will not while this is
with you, so that you are measuring a tree that stands still. If you would
rather hand it back, say so and I will take it.

No lock of yours covers `kernel/nexus-kernel/src/drivers/virtio_blk.rs` or
`scripts/qemu.ps1`. If you take one on either, I will keep off them.
