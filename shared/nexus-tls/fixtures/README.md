# Test fixtures

## The private keys in here are test keys and nothing else

`rsa-leaf.key` and `ecdsa-leaf.key` are **private keys, committed on purpose**.
They exist so that `tests/handshake.rs` can start an `openssl s_server` and do a
real TLS handshake against it, and they protect nothing: they were generated for
this directory, they are in a public repository, and anybody reading this has
them.

Never use them for anything. Nothing in NexusOS trusts them at run time — the
root store a real connection uses is loaded from the disk, and these are not in
it.

## What each file is

| | |
| --- | --- |
| `rsa-leaf.der`, `.pem`, `.key` | a self-signed RSA-2048 certificate for `example.test` and `*.example.test` |
| `ecdsa-leaf.der`, `.pem`, `.key` | the same for `ecdsa.test`, with a P-256 key |
| `signed.txt` | the message the signature fixtures are over |
| `signed.pkcs1` | RSASSA-PKCS1-v1_5 with SHA-256, made by `openssl dgst -sign` |
| `signed384.pkcs1` | the same with SHA-384 |
| `signed.pss` | RSASSA-PSS with SHA-256 and a 32-byte salt |
| `signed.ecdsa` | ECDSA P-256 with SHA-256 |

Every one of them was made by OpenSSL and verified by OpenSSL before it was
committed. That is the point: a verifier checked against its own signer agrees
with itself and nothing more.
