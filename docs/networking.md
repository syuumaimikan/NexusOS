# Reaching the network

There is no socket call. A program reaches the network through a channel, and
holding an end of that channel is the whole of the authority: a program that was
never handed one cannot open a connection, cannot send a datagram, and cannot
find out what this machine's address is.

That is the same argument as for the spawn service and the filesystem, applied
to the wire. It has a consequence worth stating plainly: on this machine you can
hand somebody a program and know it cannot talk to anything, because you can see
what it was given.

## The two halves of TCP

`kernel/net/tcp.rs` **answers** connections. `kernel/net/stream.rs` **makes**
them. They are different programs with the same wire format, and everything in
the second that is not in the first is a consequence of one fact: a server knows
the peer exists because the peer spoke first, and a client has to find out.

* A **source port** has to be chosen, and chosen so that two connections never
  share one.
* The **SYN has to be retransmitted**, because the first packet to an address
  this machine has never spoken to is also what triggers the ARP that finds it —
  so losing it is the ordinary case on a cold cache, not the exception.
* There has to be a **timeout**, because a connection to an address nothing
  answers at must end in a refusal and not in a program waiting for ever.

What is deliberately absent: congestion control, window scaling, selective
acknowledgement, out-of-order reassembly. A segment ahead of a gap is dropped
and the peer sends it again — correct, and slow on a link that reorders. This
system has never run on one.

## Request and reply, and nothing else

Every message a program sends the network service is answered with exactly one
message. Nothing is pushed the other way.

That is a choice against the obvious alternative. Pushing data up as it arrives
is better for a program that is otherwise idle and worse for everything else:
replies and unsolicited data arrive on one queue in an order neither end
controls, so every caller has to handle a push landing in the middle of a
request. That is a class of bug traded for some latency, and the latency is a
twenty-millisecond poll while a fetch is in flight — which a program with a wait
set and a deadline already knows how to do, because that is how it draws a
clock.

## Talking to itself

A packet addressed to this machine's own address never reaches the card. It is
queued and handed back to the receive path, which is what makes
`http://<this machine>/` work and what makes the whole stack testable with
nothing else on the network.

It is queued rather than handled where it is sent, because handling one sends the
next: a connection to your own address would otherwise walk an entire TCP
handshake down one kernel stack, and the kernel stack is not the place to find
out how deep that goes.

## One thread, and what that cost

The stack runs on one thread: segments arrive one at a time and are finished
with before the next is looked at, so there are no locks inside a connection.

That thread waits on two things — the card and the programs asking it for
something — and getting the waiting wrong cost a whole debugging session. It
first blocked on the card alone, so a program's first request sat unanswered
until a frame happened to arrive, which on an idle network is never. Waiting on
a *wait set* of the service channels was the next attempt and was worse: a wait
set returns when a member is ready, so a signal from the card woke the thread
and it went straight back to sleep.

What it does now is read the set's signal counter, test both things, and then
block until either the counter moves or a deadline passes. The order is the
whole of the correctness, and it is the same order every careful wait in this
system uses.

## What a program gets

`shared/nexus-netclient` writes the tags and offsets out once so that nine call
sites do not each get an offset wrong. Nothing in it blocks: `open` returns
before the connection is made, `read` returns whatever has arrived. A caller
that wants to wait does so on its own terms, next to whatever else it is waiting
for — which is the right way round for a program with a window to keep drawing.
