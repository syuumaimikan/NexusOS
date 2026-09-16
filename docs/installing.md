# Installing software written for somewhere else

```
compositor: the launcher asked for unpk
compositor: started the unpacker, and lent it the downloads folder to read
            and a folder to write
unpack: DEMO.DEB is the Debian package demo-linux-app 1.2-3; 2 file(s),
        540 bytes into DEMO.DEB/
unpack: usr/bin/demo cannot run here: it asks for
        /lib64/ld-linux-x86-64.so.2, which this machine does not have
unpack: those programs are dynamically linked against a C library this system
        has none of; the files are unpacked and are not runnable
unpack: DEMO.DEB/ holds 2 file(s), 1 of which need a loader that is not here
```

Driven the way a person would: open the launcher, type enough of "Install
downloads", press return.

## The ask, and the honest half of it

The request was to download a Linux installer in the browser and be able to
install it. This is the half that can be done, and the last two lines above are
the point of the whole exercise rather than an apology at the end of it.

**Unpacking is real and it works.** Running what comes out does not, and the
reason is not a missing feature — it is that a `.deb` full of programs built
against a C library needs that C library, a dynamic loader, and usually a window
system and a driver stack besides. Each of those is larger than this operating
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
loader every program built against glibc asks for. That is what makes the last
two log lines at the top of this file evidence rather than a message.

## Limits, stated rather than discovered

Twelve megabytes of heap, and an archive above six is refused by name. Plenty of
real packages are larger than that, and being told so beats finding out.

Links are recorded and not followed: this system has no links, and inventing one
by copying the target turns one file into two that then disagree.

Nothing is executed, nothing is configured, and no package's scripts are run —
a `.deb` may carry `preinst` and `postinst` shell scripts, and this reads
neither. Unpacking is not installing in the sense a package manager means it,
and calling it that would be the same overclaim as calling the files runnable.
