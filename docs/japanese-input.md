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

## What this is not

It does not convert to kanji. That needs a dictionary of readings, a way to rank
candidates and a window to choose from them; the dictionary alone is larger than
this whole system.

Saying so matters. A machine that offered "Japanese input" and quietly meant kana
only would be a machine somebody discovers the limits of half way through a
sentence. What is here is the half that is a function of the letters — and it is
the half that has to be right first, because a kanji converter is fed kana.
