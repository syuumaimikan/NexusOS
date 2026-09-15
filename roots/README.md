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

At the time of writing that is **117 kept, 4 dropped**, and the four are worth
reading, because they are a decision rather than a shrug:

| dropped | why | what would fix it |
| --- | --- | --- |
| 3 | self-signed with RSA/SHA-1 | nothing. SHA-1 is broken and stays refused |
| 1 | a P-521 key (`1.3.132.0.35`) | a third curve, for one authority |

It was 76 until `shared/nexus-tls/src/p384.rs` existed. Thirty-seven of those
forty-five were dropped for one missing curve, four for a missing hash prefix,
and one for both -- none of them because anything was insecure, all of them
because the arithmetic was two limbs too short.

The three SHA-1 roots are the interesting ones. SHA-1 collisions have been
practical since 2017, and an authority that signs with it is one whose
signature can be forged. Supporting it to raise the number to 120 would make
this machine's trust *worse*, so it is not supported and the number stays at
117.

## Rebuilding it

Replace the file, re-run the tool, and read what it says. It warns about
anything expiring within six months, which is the point at which a store wants
renewing rather than the point at which somebody notices it has stopped working.

## What is not here

**Revocation.** An authority withdrawn by Mozilla stays trusted by this machine
until the bundle is replaced. Doing it properly means CRLs or OCSP and a policy
for what to do when neither is reachable -- and a half-implementation would be
worse than the honest gap, because it would look like the real thing.
