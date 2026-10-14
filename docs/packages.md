# Packages

A package on this machine is a signed archive in `PKG/`. The machinery for
reading one, checking it, writing it, checking what was written, and putting
everything back if any of that fails has been here since the installer was
written. Both the Store window and the native `npkg` command use that mechanism.

The package window is that. It is a list with a key bound to
`nexus_install` — not a second implementation of installing. A package
manager whose window installed things a different way from its updater would be
a machine with two ideas about what is on it.

## What a row says

| Column | What it is |
| --- | --- |
| Package | the name the package claims |
| Version | the release it claims, if it names one this system understands |
| Signature | signed / unsigned / bad signature / not a package |
| Status | not installed / update available / installed / older than installed |
| File | which file in `PKG/` it came from |

The Status column is the one worth having. A list of files says nothing a
directory listing would not; the comparison against `installed.txt` is what
turns it into a question somebody can answer.

**Unsigned and wrongly signed are different facts and are reported
differently.** One is nobody having claimed anything about the file; the other
is a claim that does not hold. A window that said "bad signature" for both would
make an unsigned package look like a tampered one, and the two call for
different reactions.

## What installing means

The same four steps it has always meant, in this order:

1. read the package,
2. verify the signature and everything the package says about itself, before a
   single byte is written,
3. write each file, then read it back and verify it again,
4. if any of that fails, put back exactly what was there before.

Step 2 guards against a package that was corrupted or tampered with. Step 3
guards against something different — a bad disk — which is why the check happens
twice and why the second one is not redundant.

Only a package signed by the key this machine trusts is installed. The window
says so itself rather than handing an unsigned package to the installer and
reporting what came back, because the installer's refusal is phrased as a fact
about a signature and the useful sentence here is about what this machine will
do.

## The record

`system/installed.txt` is `name = version`, one per line. Store, updater and CLI
put the payload and this record into the same rollback transaction. Each reads
the latest record while holding the installation lock, so one installation
cannot replace another's record with an older snapshot. A Store package with
no semantic version can still be installed without a version record.

## Native command

Type these in the NexusOS terminal:

```text
npkg info PKG/DEMO.NEX
npkg check PKG/DEMO.NEX
npkg install PKG/DEMO.NEX
npkg list
```

Input paths are relative to the terminal's current directory. Installation uses
a separately lent system-root capability; `cd PKG` followed by
`npkg install DEMO.NEX` still installs into the system root. The command receives
stdout, stdin, the writable installation root and the read-only source directory.
Other terminal programs keep their existing read-only working-directory handle.

## Application metadata

The host packer builds on Linux and Windows. Add `--manifest=app.txt` after the
release argument to include a signed `META/<package-id>.app` file:

```text
format=1
platform=linux
arch=x86_64
entry=linux/usr/bin/pkghello
```

`platform` accepts `nexus`, `linux` or `windows`; `arch` accepts `x86_64` or
`x86`. Every field is required exactly once, unknown fields are refused, and
the entrypoint must be included in the same package. The extension does not
change the existing signed package format. Plain legacy packages need no
application manifest. For example:

```text
nexus-pack example.nexus keys/development.key example 1.0.0 --manifest=app.txt linux/usr/bin/pkghello=hello.lx
```

Installing Linux ELF bytes uses the native filesystem; running them uses the
existing Linux compatibility layer. Linux executables belong under `linux/`
relative to the installation root; e.g. `linux/usr/bin/pkghello` is started with
`run linux:/usr/bin/pkghello` (`linux32:` for i386). They still require supported libraries and
system calls. Windows PE bytes can be stored and recorded, and `npkg` explicitly
reports that Win32 execution is unavailable. This is not yet an apt/pacman/MSI
replacement: repository downloads, dependency resolution, uninstall ownership,
arbitrary Linux/Windows application execution and Steam sign-in remain unfinished.

## Bounds and recovery

Packages may be 2 MiB, with at most 1,024 files and 1,024 distinct parent
directories; each file may be 1 MiB. The native callers reserve an 8-MiB heap.
Larger application distributions require a future streaming installer. All
paths are canonical and relative: empty components, `.`, `..`, backslashes,
colons and control characters are refused. Nested directories are supported.
Duplicate destinations, file/parent conflicts and `.OLD` recovery names are
rejected before destination writes. Existing directories and links are never
replaced as files.

Exclusive creation of `.INSTALL.LOCK` serializes installer callers on the same
authority root. All callers must also avoid other writes to destinations during
installation. A pre-existing lock or backup is refused rather than overwritten.
Changes are journaled in memory before target mutation, and backup bytes are
verified before replacing originals. Exact readback detects truncation, invalid
counts and changed bytes. Handles close on success and failure.

Rollback itself can fail on a damaged or full disk. Such failures are reported
and verified `.OLD` backups are retained for recovery. A crash/power loss is not
an atomic package transaction; its lock is deliberately retained. Only remove a
stale lock after confirming there is no live installer and recovering retained
backups. Successful installs report any backups that cleanup could not remove.

## What it is given

Two handles, and no others:

* the **filesystem**, read and write and transfer — packages are read from it
  and the files inside them are written to it;
* the **settings directory**, where `installed.txt` lives.

No spawner. This window installs software and cannot start any, which is worth
saying plainly: the authority to put a program on the disk and the authority to
run one are separate here, and nothing merges them.

An installer that needed to run as the system in order to write a file would be
an installer that can write a file anywhere. This one can do exactly what the
handle it was lent allows, and nothing elsewhere.

## Testing it

`scripts/test-store.ps1` boots the machine, opens Packages through Super+Space
and the WM launcher, and drives the list with the arrow keys.

The disk carries three packages on purpose: `DEMO.NEX` (demo 1.0.0, signed),
`DEMO11.NEX` (demo 1.1.0, signed) and `BAD.NEX` (one byte changed after
signing). That is what makes both answers checkable without the test having to
build a package itself.

The test presses Enter on the tampered one first and requires that nothing was
installed from it, then installs a signed one and requires that it was. The
first of those is the whole point of signing a package, and a test that only
checked the happy path would pass on a machine that installed anything it was
handed.

`cargo test --offline -p nexus-pkg` includes failure injection at every I/O
boundary, partial writes/readback, nested rollback, retained recovery copies,
record/payload recovery together, metadata validation and resource bounds.
`scripts/test-package-manager.ps1` boots its own disposable disk, drives the
real terminal, installs and executes a Linux ELF, stores a real Windows PE,
refuses a forged archive, rolls back after a later destination collision,
refuses an existing lock, and verifies the record and Linux execution after
reboot. It preserves serial logs and a PNG; `scripts/clean.ps1` prunes only
owned disposable payloads. It does not demonstrate Win32, Steam or RTX rendering.
