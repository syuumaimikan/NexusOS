# Installing software written for somewhere else

```
launch: asked for unpk
compositor: the launcher asked for unpk
compositor: started the unpacker, and lent it the downloads folder to read, a folder to write, and the Linux root to look in
unpack: DEMO.DEB is the Debian package demo-linux-app 1.2-3; 2 file(s), 540 bytes into DEMO.DEB/
unpack: usr/bin/demo asks for /lib64/ld-linux-x86-64.so.2: which is not in the Linux root
unpack: 1 of them name a loader that is not in the Linux root; those files are unpacked and are not runnable
unpack: DEMO.DEB/ holds 2 file(s), 1 of which need a loader that is not here
```

`build/unpack-test.log`, in the order it was written, with the `[user]` prefix
taken off each line. Driven the way a person would: open the launcher, type
"dow", press return. `scripts/test-unpack.ps1` is that, automated.

## The sentence that was true and stopped being true

An earlier version of the fifth line above said the loader was one
"**which this machine does not have**", and the line after it said those
programs "are dynamically linked against a C library this system has none of".

The unpacker was not checking. It said that about *every* `PT_INTERP` it saw,
because when it was written nothing on this machine could load one, so the
assumption and the fact agreed. Then another agent installed a dynamic loader at
`/lib/ld-nexus-x86-64.so.1`, and the sentence went on being printed with the
same confidence and was no longer true.

**A sentence that was right once is the hardest kind of wrong to notice**, and
nothing would have caught this: the test passed, the output read correctly, and
the only thing that had changed was the world the sentence described.

So the unpacker is lent the Linux root, read only, and **looks**. Checked in
all three directions by a temporary probe, because a function that always
answered "no" would pass the test above perfectly:

```
unpack: PROBE /lib/ld-nexus-x86-64.so.1: which is here, so it may run
unpack: PROBE /lib64/ld-linux-x86-64.so.2: which is not in the Linux root
unpack: PROBE /lib/../lib/x: which is not in the Linux root
```

The third is the `..` rule, which applies to a loader's path for the same
reason it applies to a name inside the archive: both were chosen by whoever
built the package.

Read only, and that is the point of lending a directory rather than an answer.
A program that could write into the root every translated program resolves its
libraries from could replace anybody's loader.

## The ask, and the honest half of it

The request was to download a Linux installer in the browser and be able to
install it. This is the half that can be done, and the last two lines above are
the point of the whole exercise rather than an apology at the end of it.

**Unpacking is real and it works.** Running what comes out does not, and the
reason is not a missing feature — it is that a `.deb` full of programs built
against a C library needs that C library, a dynamic loader it recognises, and
usually a window system and a driver stack besides. There *is* a loader on this
machine now, and it is not the one `demo.deb` asks for; being able to load
something is not the same as being able to load glibc's. Each of those is larger than this operating
system. See [linux-software.md](linux-software.md) for what the compatibility
layer does and does not reach.

So the machine says which loader is missing, by name, instead of mapping the
segments and jumping into a program that immediately reaches for a symbol table
nothing filled in. A sentence is worth more than a crash.

## The formats

`shared/nexus-archive`, and `gzip` in `shared/nexus-inflate` on the DEFLATE that
was already there for PNG.

| `gzip` | what a `.tar.gz` is wrapped in |
| `tar` | ustar, GNU long names, POSIX extended headers |
| `ar` | sixty bytes of ASCII per member, which is what a `.deb` is |
| `deb` | the `ar` of two compressed tars, with the control file parsed |

Both of gzip's trailing fields are checked. The checksum because it is the only
thing in the format that would notice a decompressor bug, and **the length
because it is the only thing that would notice a truncated download** — which is
the ordinary way a file off a network goes wrong.

Only gzip is read. Packages built in the last few years are usually `xz` or
`zstd`, and each of those is a whole decompressor in its own right. A package
compressed with one is refused **by name**: "compressed with xz, which this
system does not read" is something a person can act on, and "could not install"
is not.

## The names inside an archive are hostile

`safe_name` is its own function with its own tests because it is the part that
has to be right.

An archive comes from somewhere else and every name in it was chosen by whoever
made it. A name climbing out with `..` is the oldest attack there is against a
program that unpacks things, and it is still shipped in real archives by
accident as often as on purpose. An absolute path is the same problem wearing a
different hat.

Leading slashes go, `.` components go, and **any** `..` component refuses the
whole archive rather than resolving or skipping. Refusing is deliberate:
resolving `a/../b` to `b` is arithmetically correct and means a name containing
`..` sometimes works, which is a rule nobody can hold in their head while
reading the loop that uses it. And an archive containing one was made
deliberately, so unpacking the rest would be doing most of what was asked by
something that should not have been trusted at all.

## What the unpacker is lent

Two directories and nothing else.

- **The downloads folder, read only.** An unpacker has no business editing what
  it was told to read, and one that could write there is one bug away from
  rewriting the installer it is about to open. The right is taken away by the
  compositor, which holds a writable handle because the browser needs one — the
  place to narrow a right is where it is handed over.
- **A `SOFTWARE` folder, read and write.** A second folder rather than the same
  one, because what a browser wrote and what came out of it are different
  things: a downloads folder is a place where an unexpected file is not
  alarming, and a folder of installed software is one where it is.

No disk, no network, no other folder. A hostile archive can at worst fill the
folder it is being unpacked into, and it cannot leave even that.

## Why there is nothing to type

The launcher sends a four-byte tag — `brws`, `term`, `edit` — and carries no
arguments. Rather than invent a way to pass one, **the folder is the queue**:
somebody downloads an installer and then asks the machine to install what was
downloaded, which is the order the sentence goes in anyway.

Candidates are picked **by name** and identified **by content**. The suffix only
decides what is worth opening; what a file actually is comes from its bytes, so
a `.tar.gz` that is really a Debian package works and a downloads folder full of
pictures does not produce a page of refusals.

One archive that cannot be read does not stop the others. A folder holding a
good package and a half-finished download is an ordinary state, and refusing
both because of the second would be the wrong answer to the first.

## How the readers are tested

**Thirteen tests in `nexus-archive` and seven more for gzip, and every archive
in them was made by the reference tools rather than by this repository.** A
reader tested only against a writer written from the same reading of a
specification agrees with itself perfectly and with nothing else.

That choice found two real holes while the tests were being written:

- **POSIX extended headers were being read as ordinary files**, so an archive of
  one file appeared to hold two. Every tar written this decade uses them.
- **A truncated archive read as a small complete one.** The loop simply ended
  when it ran out of bytes, which is indistinguishable from a tidy ending — and
  that is precisely how half a download passes for a whole one. A tar ends with
  zero blocks; not reaching them is now an error.

The package on the disk (`assets/demo.deb`, in `DOWNLOAD`) came out of those
same tools and holds a genuine x86-64 executable whose `PT_INTERP` names the
loader every program built against glibc asks for. That is what makes the
last lines at the top of this file evidence rather than a message.

## Limits, stated rather than discovered

Twelve megabytes of heap, and an archive above six is refused by name. Plenty of
real packages are larger than that, and being told so beats finding out.

Links are recorded and not followed: this system has no links, and inventing one
by copying the target turns one file into two that then disagree.

Nothing is executed, nothing is configured, and no package's scripts are run —
a `.deb` may carry `preinst` and `postinst` shell scripts, and this reads
neither. Unpacking is not installing in the sense a package manager means it,
and calling it that would be the same overclaim as calling the files runnable.
