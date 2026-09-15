# The certificate authorities this machine trusts

`mozilla-ca-bundle.pem` is the list of root certificate authorities that
NexusOS believes. Everything `https://` on this machine rests on it: a
certificate chain is trusted exactly when it ends at one of these, and a chain
that does not is refused.

## Where it came from

| | |
| --- | --- |
| source | [certifi](https://github.com/certifi/python-certifi) 2026.07.22, which republishes Mozilla's CA list |
| taken | 15 September 2026 |
| SHA-256 | `9cc2a774b5198dcff14d9be1e66091f538975d867ce029a96bce15a55dfd730f` |
| certificates | 121 |

Mozilla's list rather than Microsoft's or Apple's because it is the one with a
public policy, a public inclusion process, and a public archive -- so a question
about why a particular authority is in here has an answer somebody outside this
project can check.

The bundle is committed rather than fetched at build time on purpose. A build
that downloaded its own root store would be a build whose trust decisions
depended on the network at the moment somebody ran it, and the downloaded file
would have to be verified against something -- which is the problem it was
supposed to solve.

## What actually reaches the machine

Not all of it. `tools/nexus-roots` reads this file, parses every certificate
**with the machine's own X.509 parser**, and writes only what that parser
accepted into `build/programs/roots.nxr`. Run it to see the current numbers:

```
cargo run -p nexus-roots -- roots/mozilla-ca-bundle.pem --list
```

At the time of writing that is **76 kept, 45 dropped**, and the reasons are
worth reading, because they are a to-do list rather than a shrug:

| dropped | why | what would fix it |
| --- | --- | --- |
| 37 | a P-384 key (`1.3.132.0.34`) | a second curve in `shared/nexus-tls/src` |
| 4 | self-signed with RSA/SHA-512 | an OID and a hash this machine already has |
| 3 | self-signed with RSA/SHA-1 | nothing. SHA-1 is broken and stays refused |
| 1 | self-signed with ECDSA/SHA-512 | the curve above, and the hash above |

Seventy-six authorities is a usable store -- it covers the authorities behind
most of the public web -- but it is not the whole list, and a site whose chain
ends at one of the other forty-five will not load. That is a real limitation and
it is written down here rather than discovered.

## Rebuilding it

Replace the file, re-run the tool, and read what it says. It warns about
anything expiring within six months, which is the point at which a store wants
renewing rather than the point at which somebody notices it has stopped working.

## What is not here

**Revocation.** An authority withdrawn by Mozilla stays trusted by this machine
until the bundle is replaced. Doing it properly means CRLs or OCSP and a policy
for what to do when neither is reachable -- and a half-implementation would be
worse than the honest gap, because it would look like the real thing.
