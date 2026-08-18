# Plan mode

Plan mode makes boxcode read-only until you have seen a plan and said yes to
it. The model reads your files, runs commands that cannot change anything, and
works out what it would do — then hands you a plan. Nothing happens to your
project until you approve one.

**When you approve, the plan is written to `plan.md` in your project.** The
model then works through it step by step, ticking each one off in the file as it
goes. Next time you open boxcode in that directory, it picks the plan up and
carries on from where it stopped — no command, nothing to select.

That is the point of the feature: a plan is a **file you own**, not a moment in
a chat window. The file being there *is* the state. Delete it when you're done.

```
  ────────────────────────────────────────────────────────────────
  PLAN  ·  ↵ send  ·  ⌥↵ newline  ·  ↑↓ history  ·  ^c exit
```

## Why

boxcode already asks before every write and every command. Plan mode exists
because that is a weaker guarantee than it sounds like.

Approving actions one at a time tells you *what* is about to happen, never *why*
or what comes after. By the eighth prompt in a row you are reading command
strings, not judging a design — and the decision that actually mattered (rewrite
the router, or add one endpoint) was made silently several prompts ago by a
model you never got to disagree with.

Plan mode moves that decision to the front, and then keeps it:

- **Disagreeing is cheap.** Rejecting a plan costs a sentence. Rejecting eleven
  writes costs eleven keystrokes and a half-modified repository to unpick.
- **You can actually read it.** A plan is a paragraph and a numbered list. The
  same change as approval popups is a few hundred lines of diff spread over
  several minutes, reviewed with a prompt waiting for an answer.
- **The agreement outlives the conversation.** The file is what the model is
  held to while it works, what you check the result against afterwards, and what
  a teammate reviews before any code exists.
- **Long work survives.** Steps are ticked off in the file as they finish, so a
  five-step job does not have to fit in one sitting or one context window.
- **`y` gets its meaning back.** Answering the same box twenty times per task is
  what trains people to press `y` on autopilot. Research needs no approval at
  all, because none of it can do anything.

## The loop

```
    /plan
      │
      ▼
  investigate ──► propose ──► you decide
   (read-only)      ▲            │
                    │            ├─ n revise ─► say what's wrong ─┐
                    └─────────────────────────────────────────────┘
                                 │
                                 └─ y start ─► plan.md written
                                                     │
                                                     ▼
                                            implement, ticking off
                                            each step in the file
                                                     │
                                   (next time you open boxcode here,
                                    it carries on automatically)
```

## Using it

**Turn it on** with `/plan`, or start a session in it with `boxcode --plan`.

**Ask for what you want**, as you normally would:

```
❯ /plan
  Plan mode on. Nothing can be written, edited, or run unless it is
  read-only — ask for what you want and you'll get a plan to approve
  first. /plan again to turn it off.

❯ add rate limiting to the items API
```

The model investigates. Reads, listings and globs need no approval, so this part
is quiet:

```
  I'll look at how the routes and config are set up first.
  · list src
  · read src/app.py
  · read src/config.py
  · $ git log --oneline -20 — 20 lines
```

**Then it proposes, and stops.** The box says the file it will write, because
approving this puts something in your project:

```
╭ Start on this plan? ──────────────────────────────────────────────╮
│  Rate limiting for the items API                                  │
│  saves to plan.md  ·  4 steps                                     │
│                                                                   │
│  Fixed window, keyed by API key — the store is single-process,    │
│  so a shared counter would be premature. 429 with Retry-After.    │
│                                                                   │
│  1. Add the limiter in src/rate_limit.py                          │
│  2. Wrap the router in src/app.py                                 │
│  3. Add requests_per_minute + burst to src/config.py              │
│  4. Cover burst, refill and the 429 body in tests/                │
│                                                                   │
│  Not doing                                                        │
│  - Distributed limiting — needs Redis, which this project         │
│    doesn't have                                                   │
│                                                                   │
│  ❯ y start                                                        │
│    n revise                                                       │
│  ↑↓ choose · enter confirm · esc revise                           │
╰───────────────────────────────────────────────────────────────────╯
```

**`n`** (or **Esc**) sends it back and returns you to the prompt, still in plan
mode. Say what was wrong:

```
❯ use a token bucket, and put the settings in the existing
  [server] table rather than new top-level ones
```

It revises and proposes again. Nothing has touched your disk yet — a declined
plan is never written.

**`y`** writes `plan.md`, ends plan mode, and starts the work:

```
  Plan approved — saved to plan.md

  Starting on step 1.
  · write src/rate_limit.py
  · ☑ Add the limiter in src/rate_limit.py
  · edit src/app.py
  · ☑ Wrap the router in src/app.py
```

Each write and command still asks for approval individually. Approving a plan
approves the *approach*, not a blank cheque.

The footer tracks where you are for as long as the plan is live:

```
  ▸ 2/4 Rate limiting for the items API  ·  ↵ send  ·  ^c exit
```

### Replacing a plan

There is one plan file per project, so approving a *different* plan overwrites
what is there. That is intended — one project, one plan — but it would throw
away work you agreed to, so the box says so before you can press `y`:

```
│  Refactor auth                                                    │
│  saves to plan.md  ·  5 steps                                     │
│  ⚠ replaces "Rate limiting for the items API" — 2/4 done          │
```

Revising the plan already in hand is not a replacement and is not flagged.

## The file

```markdown
---
title: Rate limiting for the items API
status: in-progress
created: 2026-08-11
updated: 2026-08-11
base_commit: 3c21dfb
model: deepseek-v4-flash
---

# Rate limiting for the items API

Fixed window, keyed by API key — the store is single-process, so a
shared counter would be premature. 429 with Retry-After.

## Steps

- [x] 1. Add the limiter in src/rate_limit.py
- [ ] 2. Wrap the router in src/app.py
      blocked: waiting on the config naming decision
- [ ] 3. Add requests_per_minute + burst to src/config.py
- [ ] 4. Cover burst, refill and the 429 body in tests/test_rate_limit.py

## Not doing

- Distributed limiting — needs Redis, which this project doesn't have
```

Plain markdown at the top of the project — not hidden under a dot-directory and
not in `~/.boxcode`. Commit it if you want the approach reviewed before the code
exists; add `plan.md` to `.gitignore` if you'd rather not.

**You can edit it by hand.** Reword a step, tick a box, add one, delete one —
the parser is deliberately tolerant, and misnumbered or unnumbered steps still
read back fine. The next session picks up whatever the file says. Editing the
file *is* how you change the plan without going through the model.

`status` is derived from the boxes rather than stored independently, so it can
never claim "done" over three unticked steps.

## Carrying on later

Open boxcode in a project that has a `plan.md` and it is simply used. There is
no command, because there is nothing to choose between — the file is there, so
it is the plan. The welcome panel says what it found:

```
  model     deepseek-v4-flash
  cwd       ~/code/itemstore
  plan      2/4 — Rate limiting for the items API
  next      3. Add requests_per_minute + burst to src/config.py
```

Nothing is replayed into the conversation. The plan is restated to the model on
every request instead, which is exactly what makes it resumable — it does not
depend on anything surviving in a chat history.

**Finishing with a plan is deleting the file.** A completed plan is left alone
(deleting your files is not boxcode's call) but is not followed — you get a note
on the welcome panel saying it is done, and the model is told nothing about it.

### Staleness

Each plan records the commit it was written against. If the project has moved
on, the welcome panel says so:

```
  Before you start
  plan.md was written against commit 3c21dfb; the project is now on
  9f2e1a4. Some of what it describes may have changed or already been
  done — worth checking before it carries on.
```

A warning, never a refusal. A plan agreed three weeks and forty commits ago may
name files that have since moved, and a model told to follow it will do so
confidently — saying nothing is how a stale plan turns into wrong work.

A `plan.md` that cannot be read as a plan is also reported rather than ignored,
since you are entitled to assume a file by that name is being used.

## What is and isn't allowed

| | In plan mode |
|---|---|
| `read_file`, `list_dir`, `glob` | Yes — and without an approval prompt |
| `web_search` | Yes (still asks, since the query leaves your machine) |
| `run_command`, read-only | Yes — `ls`, `cat`, `grep`, `git status`/`diff`/`log`/`show` |
| `run_command`, anything else | **Refused** — including builds and test runs |
| `write_file`, `edit_file` | **Refused**, and not even offered to the model |
| `exit_plan_mode` | Yes — always stops and asks you |

Two layers enforce this. `write_file` and `edit_file` are not in the tool list
sent to the model at all while plan mode is on, so there is no call to
intercept — and anything that arrives anyway is refused before it can become a
prompt. That second layer covers `run_command`, which stays available because
research needs it.

A refused call is not a dead end: the model is told what happened and what to do
instead, so it folds the thing it wanted into the plan rather than retrying.

### The read-only allowlist is deliberately narrow

`cargo build`, `npm test` and `pytest` are refused while planning. They are not
destructive in any normal sense, but they write to disk, and the guarantee is
unconditional — a command boxcode cannot vouch for is refused rather than
guessed about. If a build or test run is part of the work, it becomes a step in
the plan and runs after you approve.

The same allowlist governs `auto_approve_read_only`; see `tools::is_read_only`.

### What plan mode is not

It is not a sandbox, and it does not make the model trustworthy — it makes it
*inert*, which is a smaller and far more reliable claim. Once you approve, you
are back in the ordinary safety model: per-action approval, the
destructive-command banner, and the hard blocklist no setting can reach.

It also has no opinion on whether a plan is any good. It guarantees you get to
read one first. Reading it is still your job.

## Rules the implementation follows

A few invariants worth knowing, because they are what make the file trustworthy:

- **Only approval writes a plan.** A proposal on screen has touched nothing. A
  declined plan has touched nothing. Whatever is in `plan.md` was agreed to.
- **Progress is the one exception.** Ticking a step writes to the file without
  asking again — but it records work *against* an agreed plan, it never changes
  what was agreed. Prompting to tick a box would make the feature unusable.
- **A replacement is announced.** Approving a differently-named plan overwrites
  the file, and the approval box says which plan it displaces and how far that
  one got.
- **`created` is when the plan first existed**, and survives revision. Only
  `updated` moves. A genuinely different plan starts its own history.
- **A failed save is reported, not swallowed.** The approval still stands and
  the work goes ahead, but the plan stops being tracked, because progress that
  cannot be recorded must not look like progress that was.

## Interactions with everything else

- **`require_approval = false`** does not reach plan mode. That setting hands
  the model an unattended shell; plan mode is the statement that nothing changes
  until you approve a plan, and a statement with an exception is not worth
  making. The plan prompt appears even there.
- **The hard blocklist outranks plan mode.** `rm -rf /` while planning is
  reported as *blocked*, not merely out of scope — the louder reason wins.
- **Conversation is kept** when plan mode ends. Everything read while planning
  is what makes the implementation good.
- **`/new`** clears the conversation and turns plan mode off. It does not touch
  `plan.md`, and the plan is still followed — it belongs to the project, not to
  the conversation.
- **`/usage` and `/quota`** count planning turns like any others. Planning is
  usually cheaper than the alternative — reads are small, and a rejected plan
  costs one turn where a rejected implementation costs many.
