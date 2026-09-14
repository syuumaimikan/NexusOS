# Updating the machine

An operating system that cannot replace itself is one you install once and then
live with. This is how NexusOS replaces itself.

## What an update is

A signed package with a name and a version, and a version higher than the one
installed under that name. Nothing else — there is no separate "system update"
mechanism sitting beside the package mechanism, because two ways to put a file
on a disk is one way too many to get right.

## Where they come from

`PKG/` on the store, seeded from the boot image. A machine is updated by being
given a new image, which is what updating an operating system is once the
download is over.

Fetching packages across a network belongs above this and not inside it. What
arrives has to be checked the same way regardless of how it got here, and that
check is here.

## What has to be true before anything is written

Three things, in this order. A package that fails any of them is not installed,
and the machine says which one it failed:

1. **It is signed by the key this machine trusts.** The key is compiled into the
   installer from `keys/development.pub`, not read from the filesystem — a
   trusted key that lived on the disk could be replaced by anything that can
   write to the disk, which is precisely what a package installer is.
2. **Its contents hash to what its header says.** Checked before a byte is
   written, and each file checked *again* after it has been written and read
   back — the second check is not about the package, it is about the disk.
3. **Its version is higher than what is installed.** This is what makes it an
   update rather than a reinstall, and it is why a downgrade is refused: a
   machine handed an old image should not quietly go backwards.

## Two releases on one disk

A directory holds two releases of a package the moment an update arrives beside
the release it supersedes. Installing both would leave the machine at whichever
the filesystem happened to list last — a version that depends on directory
order. So one release per name is considered, the highest, and the rest are
named in the log as superseded rather than silently dropped.

`nexus_update::newest_of_each` is that decision, and it is a pure function with
tests, because it is the part that has to be right every time and the only part
that can be checked without booting anything.

## What it leaves behind

Two files in `system/`:

* `installed.txt` — what is on the machine, by name and version. This is the
  record every later run compares against, and it is why the second boot of a
  machine does nothing: it knows what it did on the first.
* `updates.txt` — what the last check found: how many are pending, how many were
  installed, how many were looked at. The desktop reads this and nothing else.

## When it runs

At every boot, from `init`, before the desktop appears — for the same reason a
desktop machine checks at every login. A machine that only updates when asked is
a machine that does not update.

`init` is the right program to do it because `init` is the one holding the
filesystem. It hands the updater two handles, the root and the settings
directory, and neither is ambient: a program handed neither could not update
anything, and one handed only the first could update the machine and then forget
it had.

## Automatic, or asked

`system.updates` in the settings file decides. `automatic` — the default, and
what an unset or unreadable value is read as — installs what it finds. `ask`
does the whole check, writes down what it found, and installs nothing.

A machine that quietly stopped updating because somebody mistyped a setting
would be the worst of the three outcomes, which is why only the exact words
`ask` and `manual` turn it off.

## What the desktop shows

One number, from `updates.txt`: how many updates are waiting, or how many were
installed this boot. The strip reads that file and does nothing else with it.
A dock that could install software would be a dock with the run of the disk, and
the desktop is handed the settings directory read-only for exactly that reason.

## Rolling back

The install is the same code `install` uses, so the same guarantee holds: every
file about to be overwritten is copied beside itself first, every file created
is remembered, and if anything fails the copies go back and the new files are
removed. An update either happened or did not.

It is not atomic against losing power — that needs the filesystem's journal to
cover a whole sequence of operations, and it does not yet. It is atomic against
the update failing, which is what actually happens.

## Testing it

`scripts/test-update.ps1` boots a machine with nothing installed, checks that it
refused the forged package, recognised the superseded release, installed the
newer one and recorded it — then boots the same disk again and checks that it
did nothing at all. An updater that reinstalls everything it finds looks exactly
like one that works, until you watch it twice.
