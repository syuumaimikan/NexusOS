# The agent

`assist` is a window you can type a question at. It is worth being exact about
what it is, because the word "AI" invites an assumption that would make every
sentence after it misleading.

**There is no language model on this machine.** Nothing has been trained, there
is no weight anywhere in this program, and no inference happens. What the
program does is read a question, decide which of a small set of *tools* would
answer it, ask permission for that tool, run it if it may, and say what it did.

That is the shape of every tool-using agent. The interesting half of one — the
half that decides what a program is allowed to do to somebody's machine — is
here in full. The half that is absent is the model that would pick the tool more
cleverly than a table of keywords.

## What it can be asked

```
> memory
Using SystemInfo (Read)
46 MiB of 1016 MiB used (4%), 14 processes, 43 threads on 4 processors

> find notes
Using FileRead (Read)
Read 9 files
system/boot.log is the closest, overlap 3 of 73

> delete note
FileRemove needs Write, and somebody has to confirm it. This window cannot ask.
```

The third one is the point. A question it may not act on is refused **with the
reason and the permission it would have needed**, not silently ignored. An agent
that quietly declines is an agent nobody can reason about.

## Permission is not this program's decision

The policy lives in `shared/nexus-ai`, which is GPT-6 Astra's. It says what
permission each tool needs and how far an agent may go without somebody
confirming it:

| Tool | Permission | Level |
| --- | --- | --- |
| SystemInfo, FileRead | Read | Safe |
| FileWrite, FileRemove | Write | Confirm required |
| TerminalExecute | Execute | Confirm required |
| ProcessStop | Process control | Confirm required |
| NetworkConnect | Network | Confirm required |
| SettingsWrite | System configuration | Privileged |
| KernelMemory | Privileged | Blocked |

This window asks `requirement` and obeys the level.

### Why `requirement` and not `authorize`

`nexus_ai_core::authorize` answers "may Astra's *service* execute this", which
folds two questions into one: whether an agent is allowed to, and whether that
service has implemented it. Only `SystemInfo` is implemented there, so
everything else comes back `Unsupported`.

This window does its own reading, with its own read-only handle, so the second
question is not about it. Asking `authorize` made it report that looking through
files was "not built yet" **while it was doing exactly that** — which is how the
distinction was found.

## What it was lent

Two handles:

* the **filesystem, read-only** — read and transfer, not write;
* the channel that says what the machine is doing.

Not the spawner. Not the network. Nothing that can write anything.

So the honest answer to "what can this agent do to my machine" is: read files in
the one directory it was handed, and ask how busy the machine is. That is a fact
about the handles, checkable from outside the program, and not a promise about
its behaviour.

It is also the narrowest set of authority anything on this machine is given, and
that is deliberate: an agent decides for itself what to do, which is exactly why
the bound on what it *can* do should be the tightest one here.

## Searching

`shared/nexus-index` hashes character three-grams into 256 buckets and compares
documents by the cosine of the angle between their count vectors. It is not a
model either — nothing is trained, and there is no floating point in it, the
comparison being done by cross-multiplying two ratios so that two machines rank
the same way.

Characters rather than words, because Japanese has no spaces and a word
tokeniser would treat a whole sentence as one token.

The walk is bounded: two directories deep, a hundred and twenty-eight files, and
sixty-four kilobytes of any one of them. An agent that walked an unbounded tree
would be an agent a deep directory could hang.

## Testing it

`scripts/test-assist.ps1` presses the seventh button on the strip and asks three
things: one it may answer, one that reads files, and one that the permission
model refuses. The third is the one worth having — a test that only walked the
happy path would pass on an agent that deleted the file.

## What would change with a model

The tool table would stay. The keyword matching would be replaced by something
that chooses better. The permission model would matter more, not less: a program
that picks its own actions from a sentence somebody typed is exactly the program
whose authority should be a handle it was lent rather than a promise it makes.
