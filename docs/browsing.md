# The browser

A window that fetches a page over HTTP and shows what is in it. It is an
ordinary client: handed a surface, drawing into memory, with no idea where that
memory ends up. What it is given that no other window is is an end of the
kernel's network channel.

## What it can show

Text. Headings, paragraphs, lists, quotations, preformatted blocks, and links
you can follow by number. No pictures, no styling, no scripts.

That is a real ceiling and it is stated rather than worked around. A machine that
can *read* the web is a different and much smaller problem than one that can
render it as designed, and this is the first problem. A great deal of the web is
readable.

## What it will not pretend to do

**HTTPS.** There is no TLS on this machine. `https://` is refused in words rather
than quietly fetched over `http`, because a browser that silently downgraded a
secure address would be worse than one that cannot open it — the person would
not know.

## The three libraries, and why they are libraries

Everything that can be got wrong without a network is in a crate that can be
tested without one:

* `shared/nexus-dns` — the question, the answer, and what to believe of it.
  Compression pointers are the interesting part: answers use them constantly, a
  reader that ignored them would fail on nearly every real reply, and a message
  from a stranger can point at itself. Every jump is counted.
* `shared/nexus-http` — a URL to take apart, a request to write, a response to
  read *as it arrives*. The incremental shape is what lets a page be shown
  before the last byte lands, and the only shape that does not need the page in
  memory twice.
* `shared/nexus-html` — a page reduced to blocks of text. There is no place in
  it where a malformed page is an error: an unclosed tag, a tag that closes
  nothing, an attribute with no value, a comment that never ends. Each has a
  defined outcome and none is a panic or an unbounded loop, which is the
  property that makes it safe to point at the internet.

Sixty-seven tests between them, none of which needs a machine.

## Where the name resolution lives

In user space, not the kernel. The kernel carries what needs the card — a bound
port and a way to send a datagram. Which server to ask, how long to wait, how
many times to try, whether a name with dots in it is a name at all or an address
somebody typed: those are opinions, and opinions in the kernel are opinions
nothing can replace.

## A fetch is stepped, not called

A name to resolve, a connection to open, a request to send, a response to read —
every one takes longer than a frame. So a fetch is a state machine the window
*steps*, gets told what happened, and goes back to drawing. Nothing in it blocks.
That is why a page can load without the window freezing, and a page loading is
exactly when somebody is looking at it.

## The home page

This machine's own web server, reached through the whole stack rather than around
it. A browser that shows it has proved the address bar, the URL parser, the
connection, the request, the server, the loop back, the reader and the layout —
in one press, with nothing else on the network.

## Saving what was fetched

`F2` writes the page to disk.

```
[user] browse: saved index.html, 253 bytes
[user] browse: saved index-1.html, 253 bytes
```

The second line is the same page fetched again. `create` and not `open`, so a
save never silently replaces a file -- two servers both saying `index` should
not mean the second quietly destroys the first -- and a number goes in before
the extension when the name is taken, because the extension is what says how to
open the thing and a browser that moved it would save files nothing will open.

### A folder of its own

Not the documents folder the editor is lent, and certainly not the disk. A
browser is the program on this machine most likely to be handed something
hostile: what it fetched is somebody else's bytes under somebody else's name,
and the one place it can put them should be a place where finding an unexpected
file is not alarming. The compositor makes `DOWNLOAD` and lends that, read,
write and transfer -- not close, so a browser cannot take the folder away from
the program that lent it.

### The name is somebody else's text

A server chooses what its URLs say. The last path component may have separators
in it, may be `..`, may be four hundred characters of nothing. Everything about
turning it into a filename is refusing rather than repairing: anything that is
not a letter, a digit, a dot, a dash or an underscore is *replaced* rather than
dropped -- so two different names cannot collapse into one -- the whole thing is
cut to sixty-four characters, and a name that is entirely dots becomes
`index.html` rather than naming a directory that already exists. A path ending
in a separator is a front page and gets `index.html` too.

### Two mistakes worth keeping

The key was in the page handler first, which meant it did nothing at the only
moment anybody would press it: after a page loads, the address bar still has the
keyboard. It is handled before the split between bar and page now, because
saving is about the window rather than about the caret.

And the handles were read one slot early. Handle nought is the surface and
handle one is the network service; the optional pair begins after those. Reading
from one made the browser treat the network service as a certificate store and
the certificate *file* as a folder -- and the only sign was `create` being
refused, which is exactly right, because a file is not a directory.

Which of the two optional handles are present is now said in the message rather
than counted, because counting is what breaks: a machine with no root store but
a downloads folder would hand the folder over in the slot where the browser
expects certificates.

### What is checked

The log says what was saved and how many bytes. Then the test reads the name out
of that line and looks for it in the host's copy of the disk -- the name from
*this* run, not a fixed one, because saving the same page twice numbers the
second and a test looking for `index.html` would find the previous run's file
and pass while this run had failed.
