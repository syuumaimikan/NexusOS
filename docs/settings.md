# Settings

There is no settings service on this machine, no registry, and nothing that has
to be told when something changes. There is a text file, `system/settings.txt`,
and there are programs that read it.

The settings window is one more program that reads it, and the second one
allowed to write it.

## What it changes

Every row names a key that something in this system actually looks at. That is
the rule the window is built on, because a window offering to change something
nothing reads is a window that lies.

| Row | Key | Who reads it |
| --- | --- | --- |
| Background | `look.style` | the wallpaper |
| Top, bottom, accent | `look.top`, `look.bottom`, `look.accent` | the wallpaper, the desktop strip, and — through the kernel — the compositor's window frames |
| Language | `system.language` | the desktop, at startup |
| Timezone | `system.timezone` | the desktop's clock |
| Updates | `system.updates` | the updater, when it decides whether to ask |
| Connection | `network.mode` | the kernel, when it brings the network up |
| Name | `user.name` | the desktop |

`network.mode = off` is the one row on this list that is a security control
rather than a preference: it is read by the kernel and it is how a machine is
given no network at all.

## How a change travels

Nothing is pushed. The settings window rewrites one line of a file; everything
else finds out by looking, on a clock it already had:

```
settings window ──writes──▶ system/settings.txt
                                    │
              ┌─────────────────────┼─────────────────────┐
              ▼                     ▼                     ▼
       wallpaper (2 s)      desktop strip (1 s)    kernel (at network bring-up)
```

The cost of that is latency measured in seconds and the benefit is that there is
no subscription to leak, no service to restart, and no program that has to be
running for a setting to take. A machine where changing a setting needs
something to be restarted is a machine where settings are a restart.

The one thing it does not reach is the interface language of a window that is
already open. The language a program draws in is chosen when it starts, except
for the F1 broadcast the kernel sends through the compositor; a window opened
after the setting changed is in the new language, and one opened before is not.

## Two kinds of row, and when each is written

A **choice** — the background, the language, the timezone, updates, the
connection — moves along a list with the left and right keys, and is written the
moment it moves. There is nothing half-done about picking the next item in a
list.

A **typed value** — the three colours, and the name — is edited in place and
written when Enter is pressed. Saving on every keystroke would put `4`, `40`,
`40d` into a file three other programs are reading, and two of those three would
fall back to a default for each of them.

Editing a typed value starts from what is already there, so changing one digit
of a colour does not mean typing all six. Escape gives up the edit; moving to
another row gives it up too, because leaving a half-typed value by walking away
from it is how somebody says they did not mean it.

A colour that is not a colour is refused and *kept in the field*: somebody who
mistyped one digit wants to fix that digit, not type the other five again.

## What it is given

One handle: the settings directory, with read, write and transfer on it — not
close, so the window cannot take the directory away from the compositor that
lent it. It has no filesystem handle, no spawner, no network. The most a
compromised settings window can do is write nonsense into one file, and every
program that reads that file already treats an unreadable value as the default,
because [`nexus-look`](../shared/nexus-look/src/lib.rs) and
[`nexus-config`](../shared/nexus-config/src/lib.rs) were written on the
assumption that the file might be wrong.

The first-run wizard is the only other program given that directory with write
on it, and for the same reason: changing the machine is what it is for.

## Reading before writing

A save re-reads the file, sets one key, and writes the whole thing back. It does
not write out what the window remembers from when it opened. Something else may
have changed a different line since — the updater records when it last looked, the
terminal's `set` writes the same file — and a settings window that quietly
reverted somebody else's change would be worse than one that could not write at
all.

The file has no truncate, so a save removes and recreates it. A shorter file
written over a longer one would otherwise keep the old ending.

## Testing it

`scripts/test-settings.ps1` boots the machine, moves the pointer to the fourth
button on the strip, clicks it, and drives the window with the arrow keys — then
waits for the *wallpaper* to say it noticed. Four programs are involved and none
of them is told anything.

It also types something that is not a colour and requires that it never reaches
the file.

The test does not assume which style the machine starts on, because the machine
remembers between runs. It reads the style the window chose out of the log and
then waits for the wallpaper to name the same one, which is the property worth
checking: not that a particular word was written, but that what one program
wrote is what the other read.
