# Turning the machine off

Four buttons at the right-hand end of the strip: **Sleep**, **Restart**,
**Off**, **Log out**.

Off and Restart genuinely stop the machine. `scripts/test-power.ps1` is the one
test in this repository whose pass condition is that **QEMU exits** — a kernel
that writes the ACPI sleep register and carries on running has not shut anything
down, however healthy its log looks, and a log is all the other tests have to go
on. Restart is checked the other way round: that run keeps QEMU's reboot enabled
and requires a *second* bootloader banner in the same log.

## Why it is a channel and not a system call

A system call cannot be withheld. Every call in the kernel's syscall table is
available to every process that runs, so there is no way to give one program
`Shutdown` and not another.

"Stop the machine" is not a power every program should have. So it arrives the
way every other power on this system does: the compositor is lent a channel to
the kernel's power service, and hands it on to the program with the button on
it. Nothing else can ask.

The compositor never uses it on its own initiative. A compositor that could turn
the machine off by itself would be a compositor with an opinion about when
somebody has finished, which is not a decision anything drawing windows should
make. The desktop asks; the compositor passes the ask on; the kernel acts.

## What ACPI gives without an interpreter

ACPI has two halves, and the division decides everything on this page.

One half is a set of **fixed registers** described in the FADT: a port number in
a table, written with a value from a table. The other half is **AML** — a
bytecode language with a namespace, methods, operation regions and a mutex model
— in which everything else is written.

| | Where it lives | Here? |
| --- | --- | --- |
| Shutdown (S5) | `PM1a_CNT_BLK`, a fixed register | yes |
| Reset | `RESET_REG`, a fixed register | yes |
| The S5 sleep type | `\_S5_`, an AML *package of constants* | yes, by shortcut |
| Battery charge | `_BST`, an AML **method** | no |
| Lid switch, thermal zones, fan control | AML methods | no |

`\_S5_` is the one piece that lives on the AML side and can still be read
without an interpreter, because it has to be a package of constants — firmware
evaluates it during shutdown, when almost nothing else is running. So it is
found by looking for its name in the DSDT and parsing the handful of bytes after
it.

That is a shortcut and `acpi.rs` says so in those words. Every step is checked
and any surprise gives up, because what is being parsed is bytecode that this
kernel does not otherwise understand, and a wrong answer writes a wrong value to
a hardware register.

## The battery, honestly

**There is none, and there cannot be one without an AML interpreter.**

Reading a battery means evaluating `_BST`, which means executing AML: a
namespace, method invocation, operation regions that map onto the embedded
controller or SMBus, and a mutex model. That is thousands of lines and it is a
different project from the rest of this one.

The power service therefore reports *no battery* rather than a number. On a
desktop, and under QEMU, that is not an evasion — it is the truth. What it is
not is a laptop battery meter, and the service has a bit for "battery level
known" precisely so that a program can tell "there is no battery" from "nobody
asked".

## Sleep, honestly

**Sleep here puts the screen out. It is not S3.**

Real suspend-to-RAM saves the processor state, puts memory into self-refresh,
hands firmware a waking vector, and reinitialises every device on the way back.
Getting it wrong means a machine that does not come back, which is the worst
failure an operating system has.

What a person mostly means by "sleep" on a machine like this one is: the screen
goes dark and stops doing work until I touch it. That needs nothing from the
kernel — the scheduler already idles every processor with nothing to run — and
it needs the compositor to stop drawing, which is a decision about what is on the
screen and therefore the compositor's.

So it is implemented in the compositor, and `power.rs` does not pretend to a
power state it does not enter. The screen is filled black, compositing stops,
and anything at all — a key, a click, a window finishing — brings it back with a
full repaint. Nothing is saved first: waking repaints the whole screen from the
windows themselves, which is what a compositor does anyway.

## The order things are tried

Shutdown:

1. Ask firmware to hand ACPI over, if `SMI_CMD` says it has not already. (On a
   machine booted through UEFI this has happened before the kernel runs.)
2. Write `SLP_TYP << 10 | SLP_EN` to `PM1a_CNT_BLK`, and to `PM1b` if there is
   one. The sleep type comes from `\_S5_`, or is 5 — which it is on the
   overwhelming majority of machines — if the DSDT could not be read.
3. The emulator ports: `0x604` (QEMU), `0xB004` (Bochs), `0x4004` (VirtualBox).
   Named as what they are. On hardware that does not claim them, an I/O write to
   an unclaimed port is discarded.
4. Say that the machine will not turn itself off, say that it is safe to switch
   it off, and halt.

Restart:

1. The ACPI reset register, if the FADT's `RESET_REG_SUP` flag says there is
   one and it is in a space this kernel can write.
2. The keyboard controller's reset line — `0xFE` to port `0x64`, after waiting
   for its input buffer. This predates ACPI and is still wired on essentially
   every x86 machine.
3. A triple fault: load an empty interrupt descriptor table and take an
   interrupt. It always works, and it is last because it gives the machine no
   chance to do anything tidy.

Under QEMU, step 2 of shutdown and step 1 of restart both work, and the test
requires that — reaching a fallback there would be a defect rather than a
difference, so `[pwr ] ACPI shutdown did not take` failing the test is
deliberate.

## Nothing is flushed on the way out

Deliberately, and worth saying because the absence looks like an oversight.

The block cache is **write-through**: every write reaches the disk before the
call returns, and the cache is updated afterwards so that a failed write can
never be served from memory as though it had succeeded. There is no dirty data
to lose, so there is nothing to flush.

## What is on screen at the end

The desktop writes "Turning this machine off…" across the strip, in place of the
buttons — which are about to stop meaning anything, and a strip full of things
to press over a screen that has gone dark invites somebody to press one.

The compositor then darkens everything **above** the strip and leaves the strip
alone. The division is deliberate: the compositor draws no text at all, and a
font in it would be a font in the one program with no business having an opinion
about words. So the thing that can draw text says what is happening, and the
thing that owns the screen makes sure it is the only thing left on it.
