# Packages

A package on this machine is a signed archive in `PKG/`. The machinery for
reading one, checking it, writing it, checking what was written, and putting
everything back if any of that fails has been here since the installer was
written. What was missing was anywhere to see it.

The package window is that. It is a list with a key bound to
`nexus_install::install` — not a second implementation of installing. A package
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

`system/installed.txt` is `name = version`, one per line. The window updates it
after an install, which is what makes the next run of the updater agree with
what the window just did. A package with no stated release is installed and not
recorded, because there is nothing to record.

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

`scripts/test-store.ps1` boots the machine, presses the fifth button on the
strip, and drives the list with the arrow keys.

The disk carries three packages on purpose: `DEMO.NEX` (demo 1.0.0, signed),
`DEMO11.NEX` (demo 1.1.0, signed) and `BAD.NEX` (one byte changed after
signing). That is what makes both answers checkable without the test having to
build a package itself.

The test presses Enter on the tampered one first and requires that nothing was
installed from it, then installs a signed one and requires that it was. The
first of those is the whole point of signing a package, and a test that only
checked the happy path would pass on a machine that installed anything it was
handed.
