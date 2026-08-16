# Deploying

You deploy by **asking**, not by typing a command:

```
❯ deploy this project to vercel
```

There is deliberately no `/deploy`. A deployment needs a provider and a target
to mean anything, and a sentence carries both — where a bare command would have
to ask for them in screens of its own before anything could start.

`deploy_project` is a tool like `run_command` or `write_file`, so this works in
the middle of a conversation. One approval prompt, then the URL — or the build
log — comes back to the model, so it can read the error, fix it, and try again.

## What it looks like

```
❯ deploy this project to vercel

  I'll deploy it to Vercel as a preview.

╭ Deploy this project? ────────────────────────────────────────────────╮
│ ⚠  PUBLISHES                                                         │
│ uploads this project to a third-party host and puts it on the public │
│ internet                                                             │
│                                                                      │
│ 🚀 vercel · Preview                                                  │
│ Vite · npm run build · dist                                          │
│                                                                      │
│ in /tmp/deploy-demo                                                  │
│                                                                      │
│ ❯ y deploy                                                           │
│   n skip                                                             │
╰──────────────────────────────────────────────────────────────────────╯

  (the deployment panel takes over here: install prompts, sign-in and the
   streaming build all happen in it)

  · 🚀 deploy → vercel (Preview) — https://my-app-x1.vercel.app

  Deployed. It's live at https://my-app-x1.vercel.app
```

A deployment **always stops for an explicit decision**, even with
`require_approval = false` — it is classified `Dangerous`, the same tier as
`rm -rf build`, because it sends your project to a third party and puts it on
the public internet. Previews are the default; the model has to be told
"production" to get it.

Once you press `y`, the deployment runs **interactively** rather than in a
headless executor. That matters, because it is what lets the two things a
deployment may need mid-run actually happen:

- **Installing a missing CLI.** The install prompt appears with the exact
  command and the guardrails' verdict on it, and waits for you.
- **Signing in.** The terminal is handed to the provider's own browser login
  and handed back afterwards.

Neither could happen inside a tool executor: one needs a prompt, the other
needs the terminal itself. So the model just calls the tool and the flow asks
you for whatever it needs, as it needs it. When it finishes, the URL -- or the
build log, if it failed -- goes back to the model, which then answers you.

Two limits worth knowing:

- **It must be the only tool call in the turn.** A deployment owns the screen
  until it finishes, so it cannot be interleaved with other calls. Asked for
  alongside others, it is declined with an explanation the model can act on.
- **It cannot set environment variables.** The schema accepts only `provider`
  and `production`, so a model has no way to name a secret, let alone invent a
  value for one. Those are typed by hand into the panel's masked field.

## The panel

Once approved, a panel takes over the bottom strip of the terminal and drives
the rest: the checklist of what has happened, any question that comes up, and
the build streaming live. The transcript stays visible above it the whole time,
and the UI never blocks.

The panel is a fixed height — the same strip the prompt normally occupies — so
the log scrolls inside it while the **status line and the keys stay pinned to
the bottom**. That is deliberate: the spinner and "esc to stop" are exactly
what you need while a build runs, and they are the first thing a panel that
simply grew would push off the screen.

```
╭ Continue with deployment? ───────────────────────────────────╮
│  ✔ Project validated (Vite)                                  │
│                                                              │
│  Project          my-app                                     │
│  Framework        Vite                                       │
│  Directory        /Users/you/my-app                          │
│  Provider         Vercel                                     │
│                                                              │
│  ❯ Yes                                                       │
│    No                                                        │
│                                                              │
│    ↑↓ choose · enter confirm · esc back                      │
╰──────────────────────────────────────────────────────────────╯
```

...and at the end:

```
╭ Deployment successful ───────────────────────────────────────╮
│  ✔ Project validated (Vite)                                  │
│  ✔ Vercel CLI 33.5.1                                         │
│  ✔ Signed in as ada                                          │
│  ✔ Project created                                           │
│  ✔ Built and uploaded                                        │
│  ✔ Confirmed live                                            │
│                                                              │
│  Deployment successful!                                      │
│                                                              │
│  🌐 Production URL                                           │
│  https://my-app.vercel.app                                   │
╰──────────────────────────────────────────────────────────────╯
```

**↑/↓** move · **Enter** confirms · **y**/**n** answer a two-choice screen
directly · **Esc** goes back a screen, and stops a deployment that is running.

## What it detects

Read from the filesystem only — no network, no package manager, nothing run:

| Detected | From | Build command | Output |
| --- | --- | --- | --- |
| Next.js | `next.config.*`, `next` | `npm run build` | provider-managed |
| Nuxt / Remix | their config or dep | `npm run build` | provider-managed |
| Astro | `astro.config.*`, `astro` | `npm run build` | `dist` |
| SvelteKit | `svelte.config.*` + `@sveltejs/kit` | `npm run build` | `dist` |
| Vite | `vite.config.*`, `vite` | `npm run build` | `dist` |
| React (CRA) | `react-scripts`, `react` | `npm run build` | `build` |
| Node.js | `main`/`start`, no build | none | none |
| Static HTML | `index.html` | none | `.` |

Rules that matter in practice: a config file on disk outranks a dependency,
and specific outranks general — a Next.js project also has React, and a
SvelteKit project also has Vite. The build command comes from your own
`package.json` `build` script where there is one, spelled for whichever package
manager your lockfile implies (`pnpm build`, `yarn build`, `bun run build`).
Output is left unset for frameworks both providers infer themselves; passing a
directory there would override a correct answer with a guess.

Everything is a default, not a decision — **"Edit configuration"** changes the
name, build command or output directory before anything runs.

## Authentication

Nothing here asks you to copy a secret into this app if it can avoid it. In order:

1. **`VERCEL_TOKEN` / `NETLIFY_AUTH_TOKEN` in your environment.** Used
   automatically, passed to the CLI through the child process's *environment*
   and never its argv (argv is world-readable via `ps`). Nothing to type.
2. **An existing CLI session.** If you have ever run `vercel login` or
   `netlify login` on this machine, `vercel whoami` / `netlify status` finds it
   and you are never asked anything.
3. **A browser login.** The app tears its own UI down, hands the real terminal
   to `vercel login` / `netlify login` so their own flow runs exactly as it
   normally would, and rebuilds the UI when it returns. No secret passes
   through this app at all.

Pasting a token into a masked field is offered as a last resort. It is kept in
memory for that one deployment and written nowhere.

A **"Sign out, then sign in"** option exists for the case a browser login alone
does not fix: a stale session from a rotated token or an account switched
elsewhere.

## The CLI

Deploying needs `vercel` or `netlify` on your `PATH`. If one is missing, it
offers to install it and shows the exact command and the guardrails' verdict on
it — `npm install -g` writes outside the project, so it is flagged as
destructive by the same `danger.rs` classifier every shell command goes through:

```
╭ CLI required ────────────────────────────────────────────────╮
│  The provider's CLI is not installed. Nothing is installed    │
│  without your say-so.                                         │
│  ⚠  DESTRUCTIVE                                               │
│  installs globally, outside the project                       │
│                                                               │
│  ❯ Yes                                                        │
│      npm install -g vercel                                    │
│    No                                                         │
╰───────────────────────────────────────────────────────────────╯
```

Nothing is ever installed without that confirmation. Set
`allow_cli_install = false` under `[deploy]` to remove the offer entirely — the
flow then tells you what to run instead of asking.

## Environment variables

Added by name, then value, with the value hidden as you type. They are passed
to the build through the child process's environment, never its argv, and:

- they are **never** printed, logged, echoed back, or written to the history;
- the variables screen shows `API_KEY = ••••••••` — a fixed-width mask, so even
  the length does not leak;
- streamed CLI output is scrubbed on the way in, so a token *the CLI* prints
  that this app never held is masked too.

> **Netlify builds locally** (`netlify deploy --build`), so these genuinely
> reach the build. **Vercel builds remotely** by default — it uploads the
> source and builds on its own infrastructure, so a variable set here reaches a
> local build step but not a remote one. For values Vercel needs at build or
> run time, set them on the project in the Vercel dashboard (or with
> `vercel env add`). This app deliberately does not shell out to `vercel env
> add`, because that CLI takes the value on the command line, where `ps` can
> read it.

## When it fails

The failure screen names the cause and offers a way forward rather than
dead-ending:

```
╭ Deployment failed ───────────────────────────────────────────╮
│  ✖  FAILED                                                    │
│  The build command failed on Vercel. The last lines of the     │
│  build log are above; run the same build locally to            │
│  reproduce it.                                                 │
│                                                               │
│  ❯ View detailed logs                                         │
│    Retry deployment                                           │
│    Cancel                                                     │
╰───────────────────────────────────────────────────────────────╯
```

A retry keeps everything already configured, including a token you supplied —
the usual reason to retry is a build error you just fixed in another window.

Recognised and explained separately: CLI missing, CLI present but broken,
signed out, credentials rejected, a name already taken, a site that no longer
exists, a build failure, a missing output directory, rate limiting, a timeout,
and a network failure. Anything unrecognised quotes the CLI's own last line
rather than inventing a summary — that is what you would actually search for.

## Deployment history

Every finished attempt is appended to `~/.boxcode/deployments.jsonl` — a plain
file you can read yourself:

```bash
cat ~/.boxcode/deployments.jsonl
```

It stores the project, path, provider, target, status, URL, timestamp and the
**names** of any environment variables. It stores no tokens and no values —
enforced by the shape of the record rather than by stripping: there is no field
a secret could go in, and `Secret` is not serialisable, so adding one later
fails to compile.

## Configuration

```toml
[deploy]
enabled = true            # false removes the deploy_project tool entirely
allow_cli_install = true  # false = never offer to install a provider CLI
history_limit = 10        # how many past deployments /deployments prints
```

## Limits worth knowing

- **This is not a sandbox either.** A provider CLI runs with your permissions
  and does what it does. The same honest caveat as the shell tool applies.
- **Cancellation kills the local process, not the remote build.** Esc stops
  `vercel deploy` on your machine; a build already accepted by Vercel keeps
  going on their side. Cancel it in their dashboard if that matters.
- **One project per run** — whatever directory the app was launched in.
  `BOXCODE_WORKSPACE=/path/to/project` points it elsewhere.
- **Windows is best-effort.** Commands route through `cmd /C` there, because
  `vercel`/`netlify`/`npm` are `.cmd` shims that `CreateProcess` will not run
  directly. That path has not been exercised on a real Windows machine.
- **Provider CLIs change.** Output parsing is defensive (JSON where it is
  offered, tolerant text reading otherwise, "unknown" rather than a guess when
  an answer is not recognised) but a sufficiently large CLI redesign will need
  the parsers updated.

## Adding another provider

One file implementing `DeploymentProvider`, plus one line in
`deploy::providers()`. Nothing in `app.rs` or `ui.rs` names a provider, so the
menus, progress panel and history pick it up with no UI change. See
`src/deploy/mod.rs` for why the trait describes commands rather than running
them, and `src/deploy/vercel.rs` for the shortest complete example.
