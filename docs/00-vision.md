# 00 — Vision

> Part of the idea-vault documentation foundation. See [README](./README.md) for the doc map.
> This document is prose only — no architecture. It fixes *what the product is for* and *what it is not*.

## The one-sentence pitch

**idea-vault** is a single-user, localhost web tool where you bring a raw idea, **run it into the
ground** in conversation with a local AI, and — when you're done — tell the AI to **store it in the
vault** so the idea (and everything you concluded about it) can be reopened and continued later.

## The problem it solves

Ideas die in three ways: they never get written down, they get written down but never interrogated,
or they get interrogated once in a chat window that is then lost forever. idea-vault targets all
three:

- **Capture** is a single action — start a new idea.
- **Interrogation** is the main loop — the AI is a rigorous, tireless foil that pushes the idea
  through every stage of thought, no matter how ridiculous the idea seems.
- **Persistence** is durable and human-readable — a stored idea is plain markdown on your disk that
  you own, can back up, and can `git` yourself, independent of the app.

## The core loop: "run it into the ground"

The name is the method. You do not want a chatbot that agrees with you. You want something that
takes an idea and:

1. **Steelmans it** — states the strongest possible version of the idea before attacking it.
2. **Stress-tests it** — premortems, cheapest-disproof-first, market/feasibility/ethics angles,
   second-order consequences.
3. **Swarms it** — when an idea deserves breadth, the AI fans out multiple subagents that each
   attack it from an independent angle, then converges their findings into one view.
4. **Concludes** — when you say you're done, it produces a consolidated writeup and extracts the
   durable conclusions into memory.

The point is not to reach "yes" or "no". The point is to have *actually thought the idea all the
way through*, and to keep that thinking.

## First-class concepts (why this mirrors an LLM harness)

idea-vault deliberately borrows the primitives of a modern agent harness and applies them to
interrogating a single idea rather than editing a codebase:

- **Memory** — durable facts and decisions extracted when an idea is stored, reloaded as context
  when it is reopened. This is what makes an idea *resumable* rather than *re-explained*.
- **Skills** — named, reusable ideation moves (premortem, cheapest-disproof, market-size, devil's
  advocate) the AI can apply on demand.
- **Agents** — specialized subagent roles (critic, researcher, synthesizer) with scoped prompts.
- **Workflows** — deterministic multi-step orchestrations (fan-out → judge → synthesize), as
  opposed to free-form chat.
- **Subagent swarming** — parallel fan-out of agents to attack one idea from many angles at once,
  then converge.

These are not implementation details hidden from the user — they are the product's surface. See
[06-concepts](./06-concepts/) for each in depth.

## Non-goals

Explicitly out of scope, to keep the tool sharp:

- **Not multi-user / not a SaaS.** It runs on `localhost`, for one person. No auth, no sharing, no
  accounts, no server deployment story.
- **Not a cloud-AI product.** The AI backend is **Ollama, local only** (`localhost:11434`).
  Everything stays on the machine. No Anthropic/OpenAI calls. (See [ADR-0003](./adr/0003-ollama-local-only-ai.md).)
- **Not a note-taking app / wiki / PKM replacement.** The vault is idea-centric, not a general
  knowledge base. Backlinks exist to connect *ideas and their memories*, not to be Obsidian.
- **Not a project manager or task tracker.** There are no due dates, kanban boards, or assignees.
- **Not a decision oracle.** It does not tell you whether to pursue an idea; it makes sure you've
  thought about it properly.

## What "done" looks like for a stored idea

A stored idea is a folder of markdown that a human can read cold, six months later, and understand:
the current best statement of the idea, the full conversation that got there, and a handful of
distilled memory facts capturing the durable conclusions. Reopening it should feel like the AI
never forgot.

## Guiding principles

- **Markdown is truth.** Anything the user reads is durable markdown on disk. The SQLite index is a
  disposable convenience. (See [ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md).)
- **Offline and private by default.** The tool must be fully usable with no network.
- **The AI is a foil, not a cheerleader.** Default posture is rigorous critique.
- **Single self-contained binary + a folder you own.** No build step, no external services beyond a
  local Ollama the user already runs.

## Related

- [11-glossary](./11-glossary.md) — precise definitions of every term used above.
- [01-architecture](./01-architecture.md) — how the above is realized as a system.
- [04-state-machine](./04-state-machine.md) — the idea lifecycle that encodes the core loop.
