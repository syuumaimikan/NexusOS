# The first time a machine is turned on

A disk that has just been made is a machine nobody has set up. It has programs
on it and an empty filesystem, and it does not know what language to speak, what
time it is, or whose it is. The first thing it does is ask.

## What decides

One file, `system/settings.txt` on the store, and one key in it:

```
system.configured = yes
```

The kernel reads it before it starts anything graphical, because the answer
changes what it starts. There is no other signal — not a flag file, not a
partition attribute, not a build-time constant. A machine is configured exactly
when that file says so, which means a machine can be *un*-configured by deleting
one file, and an image can be shipped configured by putting one there.

The kernel passes the answer to the compositor as one bit in the message that
hands over the framebuffer, together with a handle to the settings directory.
The compositor does not read the file: it holds the directory in order to give
it to the one program with business in it.

## The wizard

`user/nexus-setup` gets the whole screen, one surface, and the settings
directory. It asks seven questions — language, timezone, name, password,
confirmation, network, and a last page saying it is done — and it is driven
entirely from the keyboard, because a machine that has not been set up has not
been told it has a mouse.

Nothing else runs while it does. There is no desktop yet, and putting a wizard
in a window would mean deciding where that window goes before anybody has said
what language the title should be in.

What it writes is the settings file. What it does *not* write is the password:
what goes in is a salt and the output of PBKDF2-HMAC-SHA-512 over the password
and that salt, which is not a password and cannot be turned back into one.

The salt is not random, because this machine has no source of randomness it
would trust. It is made from the clock, the uptime at that instant, and the name
that was typed. That is weaker than random, and it is said here rather than
hidden: what a salt has to do is differ between installations, and this does.

## The desktop, afterwards

`user/nexus-shell` is handed the same directory, read-only, and reads three
things out of it: who owns the machine, what language it is in, and what
timezone it is in. It draws the first two in its strip and uses the third for
the clock.

Read-only on purpose. A dock that could rewrite the machine's settings is a dock
that can lock somebody out of it, and drawing a clock does not need that.

## Ending a session

Nothing ends a session except somebody ending it. A desktop whose last window
closes is still a desktop; it stays up, empty, with a strip that can start
something new. The two ways out are the button at the right of the strip and
F10, and both go to the compositor, which is the program that owns the display
and therefore the only one that can give it back.

This matters more than it sounds. The compositor used to stop when its last
client did, which made the machine a demonstration rather than a desktop: it was
finished the moment you closed the thing you were looking at.

## Testing it

`scripts/test-setup.ps1` boots a disk nobody has configured, types the answers
through QEMU's keyboard — the same path a real keystroke takes, through the 8042
controller, the kernel's decoder, the compositor's routing and into the program
— and then boots the same disk again to prove the answers stuck. A wizard that
runs perfectly and does not persist its answers is a wizard that runs again
every morning.

`scripts/configure-disk.ps1` is the same typing without the assertions, for the
tests that need a machine somebody has already set up.
