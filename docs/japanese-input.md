# Typing Japanese

Romaji in, kana out, as the letters arrive. `k` shows nothing yet, `ka` commits
か, `kk` commits っ and keeps the `k` waiting, `n` waits to find out whether the
next letter makes it な or leaves it ん.

F2 in the terminal moves between plain letters, hiragana and katakana. The mode
appears in the prompt when it is not the plain one, so a machine nobody is
typing Japanese on has a prompt with nothing extra in it.

## Why the waiting is the whole problem

A converter that worked on a finished word would be easy and useless. What
somebody typing wants is to see the kana appear under their fingers, which means
every letter has to be answered immediately with either "here is a kana" or
"still deciding" — and the deciding has to be right about the cases where one
letter changes what the previous two meant.

Three rules carry almost all of it:

* **A doubled consonant is っ**, and the second letter survives to start the next
  kana. `kitte` is きって.
* **`n` before a consonant is ん**, consuming *one* letter. That includes a
  second `n`, and that case decides the rule: `minna` is みんな and `annai` is
  あんない. A `nn` that consumed both would give みんあ, which is why there is no
  `nn` in the table.
* **Longest first.** `kya` has to be found before `ky` would be, and `shi`
  before `sh`. The table is walked in order, so its order is its meaning.

The price of the second rule is that `nn` alone finishes as んん rather than ん.
That is the honest trade: those words are common and typing `nn` and stopping is
not.

## Letters that can never become kana

`q` starts nothing in the table, so it is given up on at once and goes into the
line as itself. Without that it would sit at the front for ever and every letter
after it would be stuck behind it.

The giving-up and the converting alternate rather than running in sequence,
because giving up on a letter changes what is at the front: `b.` converts
nothing, and after the `b` goes through as itself the `.` is a full stop the
table knows.

## Katakana is the same table

Shifted by the ninety-six code points between the two blocks, applied at the end.
A second table would be the same data with one more chance to disagree with
itself — and the sokuon and the `n` rule produce hiragana too, so converting once
covers all three.

## And kana to kanji

```
[あ] /> echo 漢字
漢字
```

That was typed as `kanji`, converted with the space bar, and committed with
return. Space again would have moved to 感じ, and again to 幹事, and escape
would have put かんじ back.

Romaji to kana is a function of the letters. Kana to kanji is a function of
nothing: `かんじ` is `漢字` or `感じ` or `幹事`, and only meaning decides.
Which means a dictionary.

### How big the dictionary is, said plainly

**225 readings**, written out by hand in `shared/nexus-ime/src/dictionary.rs`.
A real Japanese input method ships between a hundred thousand and a million,
with part-of-speech tags, connection costs between adjacent words, and a
language model over the whole sentence. This has none of that.

What it does have is every entry readable by somebody who knows the language,
and so checkable. A downloaded dictionary would be a file nobody in this
repository can read, which is a file nobody can check. The day this wants a
hundred thousand entries, the thing to add is a *loader* -- a dictionary file on
the disk, with its own format and its own checks -- and not a larger literal.

And the consequence is stated where somebody meets it: a word that is not in the
table converts to itself, in kana or in katakana. That is a real answer. A
converter that silently mangled what it did not know would be worse than a small
one that says so.

### Segmenting

An exact match is used whole. Failing that, the reading is cut at the longest
entry that starts it and the rest converted the same way. Greedy longest-match
is the crudest segmentation there is: it gets `にほんご` right by taking the
four-kana word over the three-kana one, and it will get a sentence wrong. What a
real input method uses instead is connection costs and a language model. Neither
is here, and neither is pretended at.

### The composition

Kana goes into the line as it is typed, so by the time somebody presses space to
convert, those characters are in a line the input method has never seen. So it
keeps a copy -- the *composition* -- and whoever is being typed into replaces
that many characters when a candidate is chosen.

Getting that wrong was the bug that took a boot to find. The terminal called
`settle`, which forgets the composition, on *every* keystroke rather than only
when a conversion was open -- so each letter threw away the kana before it, and
`kanji` arrived at the converter as `じ`. It converted perfectly. It converted
the wrong thing.

### The font had to be told

The first working conversion drew as □字.

The conversion was right; the face had never heard of 漢. `shared/nexus-font`
rasterises from the build machine's own licensed copy of MS Gothic, and it
rasterises exactly the characters `charset.txt` asks for -- which listed the
kana, because typing can produce any kana, and three kanji. Conversion can
produce 358, and now the file says so, generated from the dictionary rather than
chosen: the set of characters conversion can put on screen is exactly the set
its candidates are made of, and keeping those two in step by hand is keeping
them out of step.

## What is still not here

No candidate *window*. The chosen candidate goes into the line and space moves
to the next one, which is what an input method feels like; what is missing is
the list of all of them at once, which needs a popup the compositor does not
have a shape for yet.

No learning. The order candidates come in is the order they are written in the
table, which is a judgement rather than a measurement, and pressing space three
times today will press it three times tomorrow.

No per-clause conversion. A long phrase converts as one unit or as whatever
greedy segmentation makes of it; there is no way to say "this part is right,
convert the rest differently", which is what the arrow keys do in a real input
method.
