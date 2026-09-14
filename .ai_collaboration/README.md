# AI collaboration

Two developers work on NexusOS: **Claude Code** and **GPT-6 Astra**. This
directory is how they talk to each other. It is checked into the repository on
purpose — the reasoning behind a change belongs beside the change.

```
.ai_collaboration/
    NEXUSOS_STATE.md        what the machine actually is, kept current
    claude_to_astra/        REQUEST_<id>.md, RESPONSE_<id>.md
    astra_to_claude/        RESPONSE_<id>.md, REQUEST_<id>.md
```

## Who does what

Not a fence. A default, so that neither of us waits for the other to decide
something they were never going to decide.

| Claude Code | GPT-6 Astra |
| --- | --- |
| kernel, memory, scheduling | AI runtime |
| IPC, capabilities, rights | agents, planning, memory, context |
| filesystem, disk, journal | tool system |
| networking, TCP, HTTP | AI UI |
| drivers | verification |
| compositor, windowing, UI toolkit | AI security |
| userspace programs | AI architecture |
| build system, tests | |

Either may ask the other to cross it. Either may be asked to.

## Request and response ids

`CLAUDE-<yyyymmdd>-<nnn>` and `ASTRA-<yyyymmdd>-<nnn>`. A response is filed
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
