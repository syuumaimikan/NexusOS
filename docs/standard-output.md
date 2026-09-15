# Somewhere for a program to write

```
/> ls
PICTURES/
PKG/
SYSTEM/
demo/
init/
linux/
system/
/>
```

That listing was produced by `BIN/LS.ELF`, a separate process, and the shell
read it off a channel while the program was still running. Until this existed
it could not have been: a program had nowhere to put text a person was meant to
read, so `ls` was a method on the terminal, and so was everything else a shell
does.

The roadmap had it as one line for a long time — *"the thing that would let `ls`
stop being built into the terminal"* — and estimated it as three inventions: a
directory a child inherits, a standard output, and a pipe. It turned out to be
one.

## What was missing was one line in the kernel

The spawn service already gave every new program a channel back to whoever
started it, and a message on a channel could already carry handles. The spawn
service simply threw the caller's handles away.

It does not any more. Whatever the asker attaches to its request becomes the new
program's, numbered from two — handle one is always the channel back to the
parent. That is the whole mechanism. There is no `stdout` object, no pipe type,
no new system call.

| 1 | the channel back to whoever started it; the arguments are its first message |
| 2 | standard output |
| 3 | standard input |
| 4… | whatever else the caller lends, by arrangement between the two |

`nexus_user::print` and `println` write to handle two. A program started by
`init`, with nothing attached, has no handle there, and `print` returns
`NotFound` — which is the truth, and is what `ls` falls back to the log for.

A pipe is two conventions meeting: one program's handle two and the next one's
handle three are the two ends of one channel, and neither program knows which.

```
/> ls | count
7 lines, 50 bytes, 7 messages
```

`count` is the other half — `user/nexus-count`, which reads its standard input
to the end. Both programs are started before either is waited for, and that
order is not a preference: a channel holds a bounded number of messages, so a
left-hand program writing more than that stops until somebody reads, and the
reader would be a program the shell had not started yet.

Any number of stages, not two:

```
/> ls | grep PKG | head 1
PKG/
```

Every stage is resolved to a program before any of them is started, so a line
with a mistake in its last stage does not run its first; and every stage is
*started* before any is waited for, because the other order deadlocks.

Built-in commands cannot be piped, and say so. They write into the window
rather than to a handle, and a pipe that quietly dropped the right-hand side of
`help | count` would be worse than one that refuses.

## The programs there are now

| `ls` | a directory, one name a line, directories with a slash |
| `cat` | a file, or its input, so `ls \| cat` works |
| `head` | the first few lines, and it stops reading rather than reading it all |
| `grep` | the lines containing some text -- a plain substring, not a pattern |
| `count` | how many lines, bytes and messages arrived |

`cat`, `head` and `grep` are one crate with three binaries, because what they
share -- reading lines off a channel and writing lines back -- is most of what
each of them is. They are still separate processes with separate handles; only
the source is shared.

`grep` matching a substring rather than a regular expression is worth saying
out loud. Somebody who types `grep "a.c"` on another system means a pattern, and
here it means three characters. A program that silently treated one as the other
would match the wrong lines and look right doing it.

## What the shell does

Makes a channel, hands one end over as the program's standard output, keeps the
other, and **reads it while the program runs**. Not afterwards: a channel holds
a bounded number of messages, so a program writing more than that into a queue
nobody is draining would block for ever waiting for room — and the shell would
be blocked waiting for the program. The loop ends when the other end closes,
which is what the program exiting does to it.

The working directory goes too, duplicated rather than lent, because sending a
handle gives it up and a shell that handed its own directory to the first
program it ran would have no files afterwards.

## Exiting gives back what you hold

The pipe found a third bug, and it is the one that mattered most.

A program's handles were released when its thread was *reaped*, and reaping
happens on a five-second timer in the monitor thread. So the channel a program
wrote its output to stayed open for up to five seconds after the program had
exited — and a shell reading that channel waited all of it, because a closed
channel is exactly how the shell learns the program has finished.

`ls` on its own hid it: the test waited six seconds for its screenshot, which
is longer than the timer. `ls | count` did not, and the shell sat there having
read every byte, waiting for an end that a timer would eventually provide.

Exiting now closes what the process holds, at the moment it exits, which is what
it means on every other system. The entries are taken out under the table's lock
and dropped outside it, for the reason `sched::reap_finished` gives at greater
length: dropping an endpoint wakes whoever was waiting on the other end, and
doing that while holding the lock is a deadlock waiting for a second process.

## Two more bugs, both about a thing not being there

**A handle needs the right to be passed.** The copy of the directory was made
with `READ`, and a handle without `TRANSFER` cannot go in a message — so the
send failed whole, taking both channel ends with it, and the program never
started. The shell said "no programs", which is one of three different failures
that message covered. It says which one now.

**A program with no arguments waited for them for ever.** The spawn service sent
the arguments message only when there were some. `ls` reads its arguments first,
so `ls` with nothing after it blocked on a message that was never coming. The
service now sends the message whenever the caller asked for arguments at all,
even when they are empty — and still sends nothing when the caller's request had
no argument section, because a program like `nexus-hello` expects its first
message to be a *reply* and would be broken by one that is not.

The difference between "no arguments" and "not using arguments" is one zero byte
in the request, and it decides which of those two programs works.

## What is checked

`scripts/test-stdout.ps1` types `ls` and requires: the spawn happened, the
program was lent three things, it exited, the shell read bytes off the channel,
and the shell carried on afterwards. Between them those rule out the three ways
this could look right and be wrong — the program not starting, the program
starting and writing nowhere, and the shell quietly listing the directory
itself.

It does not assert on pixels, and that is worth saying because it was tried.
Counting lit pixels does not work here: the wallpaper is a *recording*, so a
whole-screen count moves by tens of thousands of pixels a second on its own, and
the terminal's window is placed by slot, so a fixed rectangle inside it is on
the desktop on the next run. `-Shot` takes the picture at the top of this file
for a person to look at.
