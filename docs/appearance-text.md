# Fonts, and soft edges

Two settings decide what text on this machine looks like:

| Key | Values | What it does |
| --- | --- | --- |
| `look.font` | `crisp`, `smooth` | which face text is drawn in |
| `look.smooth` | `no`, `yes` | whether its edges are blended |

Both are in the settings window, under Appearance, and both default to the
values this machine had before either existed. Turning them on is a decision
somebody makes, not one made for them by an upgrade.

## Why there are two faces rather than one face with a switch

This was the surprise in building it, and it is worth writing down.

The default face is MS Gothic at seventeen pixels. At that size MS Gothic draws
from **hand-tuned embedded bitmaps**, and an embedded bitmap has no
anti-aliasing in it: every pixel is on or off. Capturing coverage from it
produces a table of nothing and everything. That is not a failure — it is
exactly what a crisp face is, and it is why small text on this machine has
always been sharp — but it means anti-aliasing cannot be applied to it
afterwards. There is nothing to soften.

A face with soft edges has to be *asked for at rasterisation time*, from a
different renderer:

* **crisp** — GDI's `TextRenderer`, which uses the embedded bitmaps.
* **smooth** — GDI+'s `DrawString` with `TextRenderingHint::AntiAlias`, which
  ignores them and rasterises the outlines with grey coverage.

Two attempts came before that one, and both were wrong:

1. *A different font for the smooth face.* Yu Gothic UI and Meiryo do render
   with soft edges — and their CJK glyphs are too wide for a sixteen-pixel cell,
   so the generator's fitting logic dropped to eight or nine pixels and the
   smooth face came out half the height of the crisp one.
2. *The same font with `AntiAliasGridFit`.* GDI+ still used the embedded
   bitmaps. Not one pixel of the output had an intermediate value.

`AntiAlias`, with no grid fitting, produced a face where 76% of inked pixels are
partially covered, at the same em size, the same cap height and the same
advances as the crisp one. Same font, two renderers.

## The format

A glyph is sixteen rows of sixteen four-bit coverage values — leftmost pixel in
the most significant nibble, so a row is exactly a `u64`. A hundred and
twenty-eight bytes for a full-width glyph.

Sixteen levels rather than 256 because that is as much as the eye asks for at
this size and a quarter of the bytes. Coverage of fifteen widens to 255 by
multiplying by seventeen rather than by shifting: a shift leaves full ink at 240
and every letter slightly grey.

The two tables cost about a hundred kilobytes in every program that draws text.
They are `static` and not `const`, because a `const` array is copied at each use
site and what the renderer wants is a reference to one row of one glyph.

## How a program draws with it

```rust
canvas.set_text_style(
    nexus_ui::font::Face::parse(Some(look.font.name())),
    look.smooth,
);
```

The style is on the canvas rather than passed to every call, because it is a
property of the surface and not of one string: a window with two labels in
different faces is a window nobody asked for.

`nexus_look::Font` and `nexus_font::Face` use the same two words, which is what
lets a program convert between them without either crate depending on the other.

**The face and the blending travel together and are still two settings.** They
are not the same question: the crisp face is full coverage everywhere it has
ink, so blending it changes nothing, and the smooth face thresholded is a face
designed for blending with its blending taken away — which looks worse than
either. Somebody who wants the crisp face and no blending is asking for what
this machine did before there was a choice, and that is the default.

The kernel draws its panel and boot logo thresholded whatever the setting says.
Blending means reading the framebuffer back, which over an uncached
write-combining mapping is far slower than writing to it, and nothing on a panic
screen is improved by softer edges.

## Rounded corners

`Canvas::fill_rounded` and `Canvas::panel` draw rounded rectangles with
anti-aliased corners **whatever the text setting says**, and the two are
deliberately not one switch. Text comes from a face designed hard-edged, so
softening it is a matter of taste. A quarter-circle is not designed at all: at
this size an un-softened one is four or five visible steps, which is not a style,
it is a staircase.

Coverage is counted rather than computed. Each corner pixel is sampled on a
four-by-four grid and the fraction inside the curve is its alpha — sixteen
integer comparisons per pixel, on about `radius²` pixels per corner, and no
square root, which matters in a system with no floating point.

A host test sums the coverage over a whole corner and checks it comes to πr²/4.
Monotonicity alone would pass for the wrong shape.
