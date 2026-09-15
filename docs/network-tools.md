# Network tools

Three commands in the terminal: `net`, `lookup` and `scan`. They are in the
shell rather than in a window of their own because a shell is where network
tools live on every system anybody has used, and because the terminal already
has a command loop, line editing, history and scrollback — a window would have
been a second copy of all of that around three commands.

## What they do

```
net                     this machine's address, gateway and resolver
lookup <name>           find a name's address
scan <host> [a] [b]     which ports answer
```

`lookup` also answers to `dig`. `scan <host>` with no ports tries fourteen
common ones; with one number it tries that port; with two it tries the range,
capped at 256.

```
/> net
address   10.0.2.15
gateway   10.0.2.2
resolver  10.0.2.3
/> scan 10.0.2.15 80
Scanning 10.0.2.15 (10.0.2.15), 1 ports
80 open
1 of 1 ports answered
```

## What `scan` is, exactly

**A connect scan, and nothing else.** It opens an ordinary TCP connection to
each port and closes it. Ports that complete the handshake are listed; the rest
are counted.

That is the whole of what a system with no raw sockets can do, and it is worth
being plain about rather than vague:

* There is no half-open scan. This machine cannot send a SYN without the kernel
  finishing the handshake, because the kernel owns the stack and there is no way
  to ask it for a bare packet.
* There is no spoofed source. Every port this touches sees a connection from
  this machine's own address, in its own logs.
* It cannot distinguish a refused port from a silent one. Both look like a
  connection that did not complete before the deadline, so it claims neither and
  reports only what answered.

Four connections run at once, because the kernel holds four outbound streams and
asking for a fifth is refused. Fourteen ports take about a second.

## What `lookup` does

A real DNS query: bind a datagram port, send a question to the resolver the
machine was told about, and wait up to four seconds. The answer is checked three
ways before it is believed — it must come from the address that was asked, from
port 53, and carry the identifier that was sent. None of those is sufficient
against somebody on the path and all three are free; together they are what an
off-path forgery has to get right.

The identifier is derived from the clock and differs between two lookups in a
row, so a late answer to the first is not read as the answer to the second.

A dotted address is recognised without asking anybody.

## What the terminal is given

Five handles now: the filesystem, the spawner, the speaker, the machine
snapshot, and the network. That is the widest set anything on this machine
holds, and it is worth naming: **a shell with the disk, the spawner and the
network can do most of what this machine can do.**

It is still a decision the compositor makes, in one place, in `open_window`. The
terminal cannot help itself to any of it; if the compositor stopped lending the
network, the commands would say the shell was not lent it rather than reporting
the network down.

## What is not here

* **ping.** ICMP echo *replies* work — the kernel answers them — but sending an
  echo request needs a service the network stack does not offer yet. It is a
  small kernel addition and it is not written.
* **The ARP table.** The stack keeps one; nothing exposes it.
* **Packet capture.** There is no way to ask the kernel for frames it did not
  address to this machine, and adding one would be adding the single most
  dangerous capability here. If it is ever added it should be a separate handle
  that the compositor never lends to a shell.
* **HTTPS.** There is no TLS anywhere yet, which is why the browser is `http://`
  only.

## Testing it

`scripts/test-terminal.ps1` types all three at a prompt and checks the log:

```
term: net 10.0.2.15 via 10.0.2.2 resolving with 10.0.2.3
term: scanned 1 ports of 10.0.2.15, 1 answered
```

The values are logged as well as drawn, because what a program puts on screen
cannot be checked from outside the machine, and an address is exactly the kind
of thing worth checking.

The test scans the machine's own address and looks up a dotted one, so it needs
no route to anywhere. What is being checked is that the commands reach the right
answer, not that the internet is up.
