# Nex: a language that runs on this machine

```
/> write hi.nex print(6 * 7)
Wrote 13 bytes to hi.nex
/> nex hi.nex
42
```

Written in the shell, on the machine, and run there.

## Why it is not called Python

Because Python was asked for and cannot honestly be delivered. CPython is about
six hundred thousand lines of C and needs a C library underneath it. GCC is
millions, and needs an assembler, a linker and a target description. Either is a
year of work and neither would be *this* system's.

And a thing called "Python" that ran a tenth of Python would be worse than
nothing, because every program anybody brought to it would fail in a way they
could not predict — a language is a promise about what somebody else's code will
do, and a partial one is a broken promise with a familiar name on it.

So: a real language with a small honest name. Nobody arrives at `nex` expecting
their existing programs to run.

## What it is

A tree-walking interpreter. Characters to tokens, tokens to a tree, the tree
evaluated directly. No bytecode, no optimiser, no garbage collector.

```
fn fib(n) {
    if n < 2 { return n }
    return fib(n - 1) + fib(n - 2)
}
let i = 0
while i < 10 {
    print(fib(i))
    i = i + 1
}
```

Integers, strings, booleans and `nil`. Variables, assignment, `if`/`else if`/
`else`, `while`, functions with recursion. `and`, `or`, `not`, the six
comparisons, the five arithmetic operators. Six built-ins: `print`, `len`,
`str`, `int`. Errors carry the line they happened on.

## Decisions somebody will disagree with

**A number is not a condition.** `if 1 { }` is refused. Every language that
guessed here disagrees with every other one about what an empty string or a zero
means, and a program that reads clearly in one of them reads wrongly in the
next.

**Assigning to a name that does not exist is refused.** `x = 1` on an undeclared
`x` is a typo far more often than an intention, and a language that made a new
variable there is one where a misspelling is silent. `let` is how a variable
starts existing.

**A function cannot see its caller's variables.** A call pushes a *fresh* stack
of scopes rather than another layer on the caller's, so a function is a function
rather than a block that happens to have a name. There are no closures, which is
the price of that and is named rather than hidden.

**No floating point.** This system has none in the kernel, and a language whose
numbers behave differently depending on where it runs is one nobody can reason
about.

**`and` and `or` stop early.** `x != nil and f(x)` must not call `f` when `x` is
nil, which is the whole reason anybody writes it that way.

## Two bounds, because this is not a toy

A `while true { }` is a program somebody will write. On a machine with one
thread and a fixed stack, a runaway program is not a mistake to report — it is a
machine that has to be turned off.

So: ten million steps, and calls nested two hundred and fifty-six deep. Both end
with a sentence naming the line. Ten million is far more than any sensible
program and a fraction of a second of a loop going nowhere.

## What it cannot reach

Nothing, by itself. The interpreter has no built-in that opens a file, reaches
the network or starts a program — so the language needs no permission model of
its own. What a Nex program can do is decided entirely by what the `nex` process
was lent, which is the same rule everything else on this machine follows.

`nex` is lent four things: the channel to whoever started it, standard output,
standard input, and the one directory it may read a program from. That is all.

## How it is checked

**35 tests on the host**, in `shared/nexus-lang`: precedence in both directions,
scopes leaking neither way, recursion, `and` stopping early, division by
nothing, the wrong number of arguments, `len` counting characters rather than
bytes, errors carrying their line, and both runaway bounds.

**And on the machine**, because none of those can tell you it is on the disk,
that the shell starts it, that it reads the file it was lent, or that what it
printed comes back out of the pipe:

```
ok   from BIN/NEX.ELF at a process
ok   BIN/NEX.ELF wrote 3 bytes
ok   exited with status 0
ok   the wrong program said what was wrong and on which line
```

Three bytes is `42` and a newline. The second program divides by nothing, and
has to fail with `nex: line 1: this divides by nothing` and a status of one — a
language that reported every program as working would pass every other check.

That test found a real thing while being written, too. `/` is not a key QEMU
knows by that name, so it was silently dropped and the program became
`print(1  0)` — which the parser refused with `expected ',' or ')'`. The error
path was working; the test was typing something else.

## What it has not

No lists, no maps, no closures, no modules, no `for`, no floating point, no
standard library beyond the six built-ins. Each is a real feature and each is
named here rather than half-built.
