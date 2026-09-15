# The Nexus API

```rust
let child = Spawn::new("BIN/LS.ELF")
    .arguments("PKG")
    .output(theirs)
    .input(nothing)
    .lend(directory)
    .start(spawner)?;

let read = process::drain(mine, |text| show(text));
let ending = child.wait()?;
```

`user/nexus-api`. It was asked for as "a Nexus API like the Windows API, so
other programs can manage and run things", and the shape it took is worth
explaining, because it is deliberately *not* the shape of the thing it is named
after.

## What layer this is

`nexus-user` is the system-call layer: one function per call the kernel offers,
each a thin wrapper over the instruction. This is the layer above — the
operations a program actually performs, which are almost never one call.

Starting a program is the clearest case. It is a path and a zero byte and the
arguments in one message, two handles attached in a fixed order, a reply of
exactly two handles or a refusal as text, a wait, and an ending to interpret.
About a hundred lines — and it had been written five times, in the terminal, the
compositor, the launcher, the assistant and `init`. Five copies of a wire format
is five places for it to drift.

## Why it is not CreateProcess

The Windows API is the boundary of the system. `CreateProcess` is something any
process may call *because it is a process*: the authority is ambient, and the
call is where it is exercised.

Here it is not. `Spawn::start` takes a spawn-service handle, and a program that
was never lent one cannot start anything no matter what it imports. `path::open`
takes a directory handle and cannot reach above it. `machine::snapshot` takes
the machine service's handle. None of them can be conjured, and there is no
function in this library that reaches something the caller could not already
have reached by hand.

So this is a convenience over the handing, not a way around it. That is the one
line that had to stay true, and it is what keeps the capability model intact
while still giving programs something reasonable to write against.

## What is in it

| `process` | `Spawn`, `Child`, `drain`, `nothing_to_read` |
| `path` | `open`, `read`, `write`, `list` — several components from one handle |
| `machine` | `snapshot` — memory, processors, processes, threads |
| `arguments()` | this program's, as the first message on the parent channel |
| `given` | what the handle numbers mean: `PARENT`, `OUTPUT`, `INPUT`, `LENT` |

Text in and out is `nexus_user::print`, `println` and `read_input`, re-exported
so a program has one thing to import.

## Three details that are not obvious

**The zero byte in a spawn request is a declaration, not a separator.** A
request *with* one says "this caller uses arguments", and the kernel then sends
the arguments message even when they are empty. Without it nothing is sent —
which is right for a program whose first message should be a reply, and fatal
for one that reads its arguments first and would otherwise wait for ever.
`Spawn` always writes it.

**Handles given to `Spawn` are gone, whether it succeeds or fails.** A refusal
that left them with the caller and a success that did not would be two rules to
remember. A caller that means to keep one duplicates it first — and the copy
needs `TRANSFER`, because a handle without it cannot go in a message at all, and
the send then fails *whole*, taking the other handles with it.

**`path::open` walks one component at a time and refuses `..`.** That is not a
limitation being worked around: `nexus_user::open` takes a single name and
refuses a separator, and that is exactly what makes a directory handle mean
"this subtree and nothing above it". The walk keeps the property; there is no
path a caller can write that climbs out of the handle it started from.

## How it is known to be a real API

Because the terminal uses it. `spawn_program`, the output drain, the wait and
the `sys` command all go through this library now rather than through their own
copies of the wire format, and the tests that exercised them still pass:

    Standard output tests passed (14 checks).
    Terminal tests passed.

An API with no callers is a claim. The interesting part of replacing a caller is
that it is what finds the awkward corners — `Child` owning both handles rather
than returning a pair, `drain` handing over text in whatever pieces it arrives
in rather than lines, `nothing_to_read` existing at all — and each of those is
the shape the first real user asked for.

## What is not here yet

The compositor, launcher, assistant and `init` still have their own copies. They
work, and changing five programs at once to prove a point is how a working
system stops working; they move over as each is next touched.

No window management: a program draws into a surface the compositor lends it and
the protocol for that is `nexus-window`'s, which is its own library and already
the right shape. No networking: `nexus-netclient` is likewise already a library.
This covers what had five copies, not everything a program can do.
