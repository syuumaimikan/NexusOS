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
