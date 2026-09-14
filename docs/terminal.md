# The terminal

A window with a prompt in it, and a shell behind the prompt.

It is an ordinary client with two things lent to it that no other window gets: a
directory to work in and a channel to ask for programs. Those two handles are
the whole of what it can do. There is no path outside the directory it was
given, and no way to start a program except by asking the service that decides.

## Why the commands are built in

Because there is nowhere else to put them yet.

On a grown-up system `ls` is a program. Here it would be a program that needs the
same directory handle, started through the same service, to print into a window
it does not own. That is three mechanisms this system does not have — a working
directory a child inherits, a standard output, and a pipe — and inventing all
three to move a directory listing out of one file would be inventing them for
the sake of an aesthetic.

So the commands that only read and write files live in the terminal, and `run`
starts a real program through the spawn service. When there is a standard output
to inherit, the first group moves out and the terminal stops being special. That
is the next piece of work here, and it is named rather than hidden.

## What `run` can and cannot tell you

It starts the program, waits for it, and says how it ended. It cannot show what
the program printed: a program's words go to the log, because there is no
standard output for it to inherit. That is said in the terminal rather than
quietly omitted.

## Splitting a line

`shared/nexus-shellwords`, which is a library because it is the part of a shell
that can be tested without a window, a filesystem or a machine. Quoting,
escaping, and what happens when a line ends in the middle of either — a shell
that got those wrong would look like a filesystem that could not find files.

An unterminated quote is reported and the words still come back, because showing
somebody what their line would have meant is more use than showing them nothing.

## Keys

The arrows move the caret; up and down recall what was typed, which is what they
do in every shell and what a hand reaches for first. Page Up and Page Down
scroll the scrollback. Escape clears the line.

Arrow keys did not exist on this machine until the browser needed to scroll:
they arrive with a `0xE0` prefix and share their scancodes with the number pad,
so a decoder that ignored the prefix would scroll when somebody typed a digit.

## Sound

`beep` asks the kernel's sound service, which the terminal is lent in the same
way as everything else. What that proves is that a program with the right handle
can make this machine do something that is not on the screen — the only kind of
output a person looking elsewhere will notice.

The speaker is one bit: a square wave at one frequency, no volume and no sampled
sound. Anything above a beep needs a real audio card, which is a DMA engine, a
ring of buffers and a mixer. That is a driver, not a port write, and it is not
pretended at.
