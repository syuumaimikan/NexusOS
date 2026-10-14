# Native window manager

Nexus's compositor owns window placement, focus and surface resizing. Title
bars contain the title only; there are no close/minimize/maximize buttons or
invisible control hit areas. Keyboard commands are consumed by the compositor
and are not sent as text to another application.

| Keys | Operation |
| --- | --- |
| Super+Enter | Open terminal |
| Super+Space | Open command bar |
| Super+Shift+Q | Close focused window |
| Super+F | Maximize / restore focused window |
| Super+M | Minimize focused window |
| Super+Shift+M | Restore hidden windows |
| Super+T | Tile visible application windows with gaps |
| Super+H/J/K/L or Super+arrows | Move focus |
| Super+Shift+arrows | Move focused window |
| Super+Shift+H/J/K/L | Resize focused window |

Mouse title dragging, corner resizing and dock restoration also remain usable.
Command bar and Appearance are transient panes and are excluded from tiling.
Tiling currently arranges the visible windows on demand; automatic tiling,
workspaces and application rules remain to be implemented.

Smooth enlarged glyphs use bilinear coverage sampling. Rounded windows/panels
use 64 coverage samples per corner pixel, with wide arithmetic for large radii.
These are CPU rendering changes; they do not demonstrate RTX acceleration.

The work area excludes the top bar and dock. Resizing initializes a filtered
full-window preview; the demo clients then report complete-frame dimensions so
the native test waits for their replacement surfaces to be composited. Terminal
text starts below the shared title-bar height. The keyboard test exercises real
surface replacement and captures the resulting QEMU screen, rather than only
checking key dispatch. Run `scripts/test-desktop.ps1` after staging and refreshing
the FAT program partition. The native UI unit suite also checks large-radius
arithmetic, glyph clipping, image interpolation and existing MSAA/blur behavior.

The signed native package manager installs Linux and Windows payloads, but its
runtime support differs by format. The tested Linux ELF runs through the current
compatibility layer; Windows PE files are stored and identified, while Win32
execution is unavailable. See [package manager](packages.md).

Settings now displays PCI/USB Wi-Fi, Bluetooth and graphics devices discovered
at boot. The host's MediaTek RZ616 is identified by its bus IDs. Wi-Fi drivers,
network scanning/association and Bluetooth drivers/pairing remain unfinished;
the inventory reports presence and support status only.
