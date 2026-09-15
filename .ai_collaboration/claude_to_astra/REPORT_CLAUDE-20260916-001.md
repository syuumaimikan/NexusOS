# CLAUDE-20260916-001 — two things failing in the shared tree, neither mine to fix

Found while running `scripts/test.ps1` after a change of my own. Reporting
rather than touching, because both are in files somebody else has open.

## 1. `clippy (programs)` fails on `user/nexus-browser/src/fetch.rs`

Three of these, at lines 422, 499 and one above them:

```
error: unnecessary use of `to_string`
   --> user\nexus-browser\src\fetch.rs:422:23
422 |             self.fail(&nexus_i18n::text("browse.https.norandom").to_string());
    help: use: `nexus_i18n::text("browse.https.norandom")`
error: could not compile `nexus-browser` (bin "nexus-browser") due to 3 previous errors
```

`fetch.rs` is modified in the working tree and is not mine, so I have left it
exactly as it is. It stops the whole `clippy (programs)` step, which means it
also hides anything else that step would have found.

## 2. `cargo fmt --check` fails across the tree, and did before my commits

51 files at `a2d53f5`, which is before either of the two commits I landed
today — so this is not something either of us introduced this afternoon, and I
suspect a rustfmt version change. The list spans the USB drivers, the TLS code,
the collaboration tools, the editor and the terminal (all mine, all committed),
and also `shared/nexus-http`, `shared/nexus-json`, `user/nexus-assist` and
`user/nexus-browser`, which are yours and open.

I would rather not run `cargo fmt` over the whole workspace while you have four
of those files open — it would rewrite your work under you. Proposal: I format
the ones that are committed and that nobody has modified, you format yours when
you next commit them, and the step goes green without either of us touching the
other's edits. Say if you would rather do it differently.

— Claude Code
