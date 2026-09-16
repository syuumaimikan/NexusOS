# Drawing in three dimensions

```
launch: asked for sold
compositor: the launcher asked for sold
compositor: started the three-dimensional drawing, and gave it a surface in slot 3, 960x582
solid: all 8 faces are wound so their front is the outside
solid: 180 frames, 818 faces drawn and 622 turned away, 2278702 pixels by the processor
solid: geometry and rasteriser agreed on every face but 2 seen edge-on, the worst leaning by 12688 of 2487965
```

Six lines of `build/solid-test.log`, in the order they were written, with the
`[user]` prefix taken off each and nothing else changed. Driven the way a person would: F3, type "dra", press
return. The picture is in `build/solid.png` — an octahedron, eight flat-shaded
faces, the near ones covering the far ones, every pixel of it worked out by the
processor.

A transform, a perspective projection, triangles filled a pixel at a time, and a
depth buffer so that what is behind stays behind. That is the whole of what
drawing in three dimensions means at the bottom, and the picture it makes is a
picture of a solid.

## What this is not

**It is not Vulkan, and nothing accelerates it.**

Vulkan is an interface to a piece of hardware: an instruction set of its own, a
shader compiler, a memory manager, a command submission model and a driver stack
underneath all of it. None of that is here and none of it is within reach — it
is the same order of work as the rest of this operating system put together, per
vendor. The same is true of OpenGL.

Every pixel below is computed by the processor. The GPU on this machine is a
*display*: it shows what is put in front of it, which is what
[gpu.md](gpu.md) is about. This puts something in front of it that was worked
out in software.

That distinction is the first sentence of `shared/nexus-render3d/src/lib.rs` as
well, because a fast library with a borrowed name would be worth less than a
slow one that says what it is.

## Why the arithmetic is integer

The user target is built with `-sse` and `+soft-float`, so every
floating-point operation is a call into a software implementation. A rasteriser
does several of those *per pixel*, which is the difference between a still
picture and a moving one.

But integers are also simply the right answer here, and would be on a machine
with a floating-point unit. **An integer edge function is exact.** Exactness is
what stops two triangles that share an edge from either overlapping on it or
leaving a gap — and a gap shows as a line of background through a solid surface,
which is the classic artifact of a renderer that decides coverage with a
tolerance.

Sixteen bits of fraction in an `i32`, `i64` for the products. A product of two
16.16 numbers has thirty-two bits of fraction, so the multiply has to go through
a wider type or it is wrong for ordinary values rather than extreme ones.

## The pipeline

| `Transform` | rotation, scale and translation, as three rows of four |
| `clip::near_plane` | cut against the plane in front of the eye |
| `project` | the one divide, which is what makes distance look like distance |
| `Canvas::triangle` | edge functions, barycentric depth, and the depth test |

Three rows and not four. The fourth row of a transform matrix is `0 0 0 1` in
everything this will be asked to do, and carrying it would be twelve
multiplications a vertex spent proving it is still there. Perspective happens at
the projection, where the divide is.

### Cutting at the near plane

A triangle with any corner behind the eye used to be dropped whole. That is a
defensible small version with a cost you can see: you cannot move the eye *into*
a scene, because a wall you walk up to vanishes entirely the moment one of its
corners passes you.

A plane divides a triangle into at most a triangle and a quadrilateral, and a
quadrilateral is two triangles — which is why the cut returns up to two pieces
and never more.

The plane sits a quarter of a unit **in front of** the eye rather than at it. At
exactly zero the projection divides by nothing, and a corner landing a hair in
front of the eye projects to somewhere far off the screen: a triangle stretched
across the whole display for one frame.

## The reflection that catches everybody

`project` turns the world the right way up — up in the world is up the screen,
which is *down* in memory. That is a reflection, and **a reflection reverses the
sense of a turn.**

So a face wound counter-clockwise seen from outside a solid arrives at the
rasteriser wound clockwise, and clockwise on screen is what it calls
front-facing. A face table written the way every textbook writes one renders the
solid inside out.

It is said in two places on purpose: in `project`, where the reversal happens,
and in the face table of `user/nexus-solid`, where somebody will trip over it.

## How it is checked

**Thirty-four tests on the host**, and the ones worth naming are those that
catch a renderer which looks right by accident:

- **Two triangles sharing an edge leave no gap.** A seam shows as background
  through a solid; a double write is invisible until the two have different
  colours and one flickers.
- **Depth does not depend on the order they arrive in.** A renderer that only
  looks right when the furthest thing is drawn first has no depth buffer at all,
  and the two are indistinguishable in a scene that happens to be sorted.
- **Clearing forgets the depths as well as the colours**, or this frame's
  triangles vanish behind the previous frame's.
- **Cutting does not turn a face around**, compared by normals rather than by
  what reaches the screen.
- **Composition is in the order it reads**, checked both ways round, because a
  matrix product that is backwards still produces a plausible picture.

**And on the machine**, which is the output at the top of this file. The path
`scripts/test-solid.ps1` drives is the whole of it: a key, the launcher, a
compositor that hands out a surface, and a program that fills it.

`user/nexus-solid` checks itself in two ways, and the difference between them is
the most useful thing in this document.

### The check that does not work

For every face of every frame it works out from the geometry whether the face
*should* be visible — whether its outward normal leans towards the eye — and
requires that to agree with whether the rasteriser drew it.

**That check is blind to the winding, and I found out by reversing the table and
watching it pass.** Both sides of the comparison are computed from the same face
table: reverse it and the computed normal reverses too, so the two still agree,
the program reports success, and what is on screen is the inside of the shape.
The run printed `622 faces drawn and 818 turned away` — the exact mirror of the
`818 and 622` above, the same 2278702 pixels, and a cheerful verdict.

It is not worthless. It is what would catch a projection that stopped reversing
handedness, or a rasteriser whose idea of a front face changed sign. It is
simply not a check on the winding, and it was written as though it were.

This is the *second* time a self-check here has been too weak in exactly this
way. The first counted faces drawn against faces turned away and passed when
both were non-zero — and a convex solid shows about half its faces from any
direction, so reversing every face leaves both counts looking healthy. Both
versions failed for the same reason: **the quantity being checked was derived
from the thing under test.**

### The check that does

The outward direction has to come from something the face table cannot move.
This solid is centred on the origin, so the direction from the centre to a face
*is* its outward direction, and the centroid of three corners is a point on that
ray. The winding is right when the normal the corner order produces leans
against it.

Reversed on purpose, that check says:

```
solid: FAILED: 8 of 8 faces are wound inside out, so what would be drawn
       is the inside of the shape
```

before a single pixel is drawn, because a table wound inside out makes a picture
and nothing later in the program would notice a picture.

### The two faces that are allowed to disagree

A face seen edge-on projects to a sliver a pixel or two wide, and the sign of a
sliver's area is decided by where its three corners round to whole pixels rather
than by which way it faces. The two methods cannot be required to agree there.

So the run reports both numbers and the tolerance is a ratio: the worst
disagreement leaned by 12688 against 2487965 for a face seen square on — half a
percent, about a third of a degree off edge-on. A fixed constant would have been
a number chosen until the test passed.

Two of 1440 face-frames, and the count is in the output so that a version of
this which started excusing something real would say so before the constant
did.

## What it has not

No texturing. No lighting model — each face is one flat colour, chosen when the
shape was written down. No smooth shading across a face, which needs a normal
per vertex and an interpolation this deliberately does not have. No clipping
against the sides of the screen in geometry, because the rasteriser clips its
own bounding box and that is cheaper than making new triangles. No sorting of
transparent surfaces, because there is no transparency.

Each of those is a real feature and each is named here rather than half-built.
