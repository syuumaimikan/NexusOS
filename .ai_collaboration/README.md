# AI collaboration

Three developers work on NexusOS: **Claude Code**, **GPT-6 Astra** and
**Gemini 3.1 Pro** (participation reported by the user on 2026-09-15). This
directory is how they talk to each other. It is checked into the repository on
purpose — the reasoning behind a change belongs beside the change.

```
.ai_collaboration/
    NEXUSOS_STATE.md        what the machine actually is, kept current
    <from>_to_<to>/         REQUEST_<id>.md, RESPONSE_<id>.md
```

A mailbox is any directory named `<from>_to_<to>`, and there are six of them
for three developers. Nothing lists them: `nexus-collab` reads the directory.

A fourth developer needs `mkdir` and no code change. That is not a convenience
— the two names used to be written into the tool's source, and when Gemini
joined, `request list` looked in two places out of six and reported a clean
inbox to somebody who had mail. A silent wrong answer is the worst failure this
tool can have, since its whole job is telling each of us what the others are
doing.

```
nexus-collab request mailboxes     all six, and which are yours
nexus-collab request list --mine   only what is addressed to you
```

## Who does what

Not a fence. A default, so that none of us waits for another to decide
something they were never going to decide.

| Claude Code | GPT-6 Astra | Gemini 3.1 Pro |
| --- | --- | --- |
| kernel, memory, scheduling | AI runtime | integration |
| IPC, capabilities, rights | agents, planning, memory, context | verification |
| filesystem, disk, journal | tool system | end-to-end tests |
| networking, TCP, HTTP, TLS | AI UI | |
| drivers | AI security | |
| compositor, windowing, UI toolkit | AI architecture | |
| userspace programs | | |
| build system, tests | | |

Any of the three may ask another to cross it. Any may be asked to.

Gemini's part was assigned by the user on 2026-09-15; see
[GEMINI_ONBOARDING.md](GEMINI_ONBOARDING.md). Verification is listed under
Gemini and no longer under Astra, which is the point of having it: **the person
who wrote a thing is the worst person to confirm it works.**

Identity is not permission. All three must read STATE and every lock before
editing, and take a task lock for the paths they are about to touch — including
for an integration fix in somebody else's file.

## Request and response ids

`CLAUDE-<yyyymmdd>-<nnn>`, `ASTRA-<yyyymmdd>-<nnn>`, `GEMINI-<yyyymmdd>-<nnn>`.
A response is filed
under the id of the request it answers, in the other party's directory:
Claude's answer to `ASTRA-20260914-001` goes in `claude_to_astra/` as
`RESPONSE_ASTRA-20260914-001.md`.

## What a request should be

Small. The protocol asks for work to be split — architecture, then API, then
implementation, then integration, then test — and the reason is that a review of
four thousand lines is not a review.

A request should carry enough that the other party does not have to guess the
state of the machine: the files, the constraints, and what "done" means. If it
does not say how the result will be checked, it is not finished being written.

## What neither of us does

Delete or revert the other's work to resolve a conflict. Inspect, understand,
compare, integrate, test — and ask if that is not enough.

## House rules that apply to both

They are in `NEXUSOS_STATE.md` and they are not style preferences:

* Build after writing. Never leave the tree broken.
* No `todo!()`, no `unimplemented!()`, no mock layer. Absent beats pretended,
  and the roadmap records what is absent.
* `unsafe` carries a `// SAFETY:` comment that says why it holds.
* Nothing is verified because it compiled. It is verified because it ran.
* Every user-visible string lives in `locales/*.txt`.
