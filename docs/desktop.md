# Native desktop

The desktop follows the three Nava OS references supplied by the user:
[trailer](https://www.youtube.com/watch?v=v2MOPr2SoD0),
[command bar](https://www.youtube.com/watch?v=HyYjPYj4nKA), and
[personalization](https://www.youtube.com/watch?v=E7nGN03tvKI).
It uses NexusOS's native compositor and applications.

- The default landscape wallpaper has overlapping pastel hills, crest
  highlights and depth shading. Existing explicit wallpaper styles and pictures
  still apply. Appearance offers `landscape` alongside the other styles.
- A translucent top bar and floating Dock are projected from the desktop's one
  shared surface. Clicks are translated back into its local coordinates.
- The command bar searches application names and existing windows. Enter
  starts an application, or restores/focuses the selected existing window.
  Up/Down choose, Escape dismisses. Long lists scroll to keep selection visible.
- Appearance uses light translucent panels, a category sidebar, colour swatches
  and existing persistent settings. Its list scrolls on smaller windows.
- Super+Space opens the command bar; Super+Enter opens a terminal; Super+C
  opens Appearance. Super+arrows cycle visible windows and Super+Shift+Q
  requests closing. Applications can decline a close request for unsaved work.
  Existing function-key shortcuts remain available. Virtual desks and the
  video's Dock auto-hide/off controls are not implemented yet.

Backdrop blur is a bounded **CPU** box blur, not RTX acceleration. A pane copies
the lower scene before blending; damage expands to its whole sampling region
and halo so repeated redraws never sample the previous blurred pane. Overlapping
panes expand damage to a fixed point. A capture and its scratch buffer are each
limited to one million pixels; allocation failure uses ordinary transparency.
Rounded edges and text preserve their coverage in an alpha layer. Opaque clients
keep their existing pixel format. Eight window slots and their auxiliary
surfaces are mapped separately from the application heap.

Text starts with the smooth face and coverage blending enabled. Programs which
read appearance settings keep explicit saved font/smoothing choices. Window corners, borders and coloured title
controls use sixteen sample centres per edge pixel. Native 3D cubes use **4x
MSAA**: each sample has independent depth and colour; samples resolve against
the real background after all faces. The top-left rule avoids shared-edge seams.
The multisample attachment is bounded to one million pixels (24 bytes each)
with fallible allocation. This is CPU rendering, not hardware acceleration.

Build with `scripts/build.ps1`, configure the disk if needed, and boot with
`scripts/run.ps1`. `scripts/test-desktop.ps1` drives the real default boot through
QEMU's monitor, checks shortcut/application/window-switch markers and saves PNG
frames and serial evidence under `build/desktop-check-*`. It does not change user
settings. Host blur/alpha tests run with `cargo test -p nexus-ui -p nexus-look`.
The wider window-control and Wayland checks are in
`scripts/test-posix-runtime.ps1 -WithWayland -WithUIControls -SkipRuntimeBuild`.
These captures wait for the actual pointer position while startup frames or a
resize are still rendering. Pointer movement damages its previous and current
positions even when the applications are idle; decoration drawing respects the
clipped damage and skips flat window interiors.

Steam sign-in and physical RTX 5060 acceleration remain incomplete; desktop
appearance and CPU effects do not establish either capability.
