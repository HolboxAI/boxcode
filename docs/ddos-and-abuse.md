# DDoS and abuse: what we can absorb, and what we cannot

**Status:** P0 shipped. P1–P4 planned, not built.
**Scope:** `boxcode.sh` (CloudFront), `auth.boxcode.sh` (one EC2 box), and the planned
full-stack hosting on Lambda.
**Verified against account `992382417943`, us-east-1.**

---

## 1. The short version

The static half of boxcode is in good shape. The dynamic half is one small box with a public IP
and no protection of any kind, and it holds a **resource-creation endpoint that needs no
authentication**.

The most urgent problem was not a DDoS. **`POST /provision` spawned a Docker container and a
Postgres database per request, unauthenticated** — roughly **40 requests from a single laptop**
exhausted the box's memory and took auth, database, change-requests and uploads down together. No
flood, no botnet, no volume. **That is now fixed** (P0 below); §3.1 keeps the analysis because the
shape of it is worth remembering.

Everything else here is layered defence for the day someone actually points volume at us.

| | Today | After §4 |
|---|---|---|
| `boxcode.sh` static | Shield Standard via CloudFront | + WAF rate rules |
| `auth.boxcode.sh` | **Nothing** — direct A record to a t3.small | Behind CloudFront + WAF, origin cloaked |
| `/provision` | ~~Unbounded container spawn~~ **FIXED** | Globally rate-limited, quota-capped, artifact-bound, reaped |
| Lambda concurrency | Shared with 42 production functions | Reserved caps, blast radius bounded |
| Detection | None | Alarms on the four signals in §6 |

---

## 2. What is actually exposed

### Verified facts

| Thing | Value | Source |
|---|---|---|
| `boxcode.sh` | `3.168.132.x` — **CloudFront** (`E2JMTKNA76TEEX`) | `dig`, `aws cloudfront list-distributions` |
| `auth.boxcode.sh` | **`34.235.117.119` — a direct A record to the EC2 box** | `dig` |
| The box | `boxcode-auth`, `i-091cf663e3c2d1a94`, **t3.small** (2 vCPU burstable, 2 GB RAM) | `aws ec2 describe-instances` |
| Its security group | `sg-0a7f3ba1f7b9dc056` — **80 and 443 open to `0.0.0.0/0`**, nothing else | `aws ec2 describe-security-groups` |
| Rate limiting in nginx | **None anywhere** — `grep -rn 'limit_req\|limit_conn' infra/` returns nothing | repo |
| Account Lambda concurrency | 1000, **all unreserved**, shared with **42 functions** | `aws lambda get-account-settings` |

*(Shield Advanced subscription status could not be read — `shield:DescribeSubscription` is IAM-denied
for this user. Assume **not** subscribed; it is $3,000/month and would be a deliberate purchase.)*

### The asymmetry that matters

```
  boxcode.sh                          auth.boxcode.sh
  ──────────                          ───────────────
  CloudFront (global edge)            34.235.117.119  ← a single t3.small
  ├ Shield Standard (automatic)       ├ no CDN
  ├ hundreds of PoPs                  ├ no Shield beyond the free L3/L4 floor
  ├ absorbs L3/L4 at the edge         ├ no WAF
  └ origin never touched              ├ no rate limiting
                                      ├ 2 GB RAM, 0.4 vCPU sustained (t3 baseline)
                                      └ runs auth + db + requests + uploads
                                        — all four die together
```

Everything stateful boxcode has is behind that second column, on one instance, reachable directly
by IP. Moving it behind CloudFront is the single highest-leverage structural change available.

---

## 3. Threats, in order of how easy they are

### 3.1 ~~CRITICAL~~ **FIXED** — `/provision` was an unbounded resource-creation endpoint

*Closed by P0. Kept here because the shape of the failure is worth remembering, and because the
same pattern — an unauthenticated endpoint that creates durable resources — is the one to check
for whenever a new control-plane route is added.*

`infra/auth/control-plane/index.mjs` accepted `POST /provision {"project_id": "abcd"}` with
**no authentication**, validated only by the shape regex `/^[a-z2-9]{4,16}$/`. Each accepted
request performed:

1. `CREATE DATABASE proj_<id>` on the shared Postgres
2. `docker run -d --network host ... supabase/gotrue:v2.189.0` — **a new container**, port from 9000 up
3. Writes an nginx conf file, then `nginx -t && nginx -s reload`
4. Rewrites the registry JSON

**The arithmetic.** A GoTrue container is roughly 30–50 MB resident. The box has 2 GB, already
running four Node services, Postgres, and nginx. **Somewhere around 35–45 containers exhausts
memory.** The id space is effectively unlimited (32⁴ ≈ 1M distinct four-character ids alone), so
each request creates a *new* container rather than reusing one.

Three separate exhaustion paths, any of which is fatal:

- **Memory** — ~40 requests.
- **nginx reload cost** — every provision runs `nginx -t`, which re-parses *every* conf file in
  the include directory. This is O(N) per request and therefore **O(N²) over an attack**. At a few
  thousand project confs, reloads take seconds and each one forks.
- **Disk** — a Postgres database and a container writable layer per id, plus a registry file
  rewritten in full each time.

**Amplification ratio: one ~200-byte HTTP request against several seconds of CPU, tens of
megabytes of RAM held indefinitely, and permanent disk.** Call it 10,000:1. This is not a DDoS
vector — it is a single-attacker DoS that needs no volume at all, and it is by a wide margin the
most serious thing in this document.

It also self-healed badly: the code deliberately does not roll back partial failures, so a
half-finished provision left a database with no container behind. The reaper now clears those.

### 3.2 HIGH — a flood at `/api/artifact` can starve production Lambdas

`POST https://boxcode.sh/api/artifact` is unauthenticated (confirmed by live probe). Each request
invokes `boxcode-artifact-signer`.

The account's **1000 Lambda concurrency is entirely unreserved and shared with 42 functions** —
`gpurouter-agent`, `fsi-genai-workshop-*`, `mach11-registration-*`, `marketplacemailing` and the
rest. A sustained flood at the signer consumes that shared pool, so **an attack on boxcode
degrades unrelated production services in the same account.**

That blast radius is the finding. The cost itself is minor by comparison.

### 3.3 HIGH — `/db/named-query` is an internal-request amplifier

No rate limiting (documented in `infra/db/README.md`). Requires a valid `access_token`, but
signing up to any project's own GoTrue is free and open, so obtaining one costs nothing.

Each call causes **an internal HTTP request** — `verifyUser()` → `GET {AUTH_BASE}/{id}/user`
against that project's GoTrue container — **plus** a SQLite open/query. One inbound request, two
units of internal work, one of them a network round trip on the same box.

The worker pool merged in #117 means a slow query no longer blocks other tenants, and
`QUERY_TIMEOUT_MS` bounds each one. That contains the *database* half. The GoTrue round trip is
still unbounded and unthrottled.

### 3.4 MEDIUM — L7 HTTP flood against `auth.boxcode.sh`

A direct A record to a t3.small with no CDN, no WAF and no `limit_req`. nginx itself is efficient,
but the four Node services behind it are single-process, and t3 burstable CPU means sustained load
**exhausts CPU credits and then throttles to 20% baseline** — after which the box is slow even
once the attack stops.

### 3.5 MEDIUM — volumetric L3/L4 against the box's IP

Shield Standard gives a free floor of L3/L4 protection on EC2, but a t3.small's own network and
CPU give out long before that floor is relevant. Nothing can be done about this while the box has
a public A record; §4.2 removes it.

### 3.6 LOW — `/uploads`, `/requests`

Both need a valid token; both have no quota or rate limit (their own READMEs say so). 5 MB per
upload with no cap is storage burn rather than an outage.

### 3.7 Not a DDoS, but same blast radius — the phishing/reputation path

Covered in the hosting plan; noted here because the response is the same runbook. A malicious page
on `boxcode.sh` risks Google Safe Browsing blocklisting the **apex domain**, which puts a red
interstitial in front of every page including the real site. Detection and takedown speed is the
only lever.

---

## 4. The plan

Ordered by value per unit of effort. **P0 is not optional and is not really about DDoS.**

### P0 — Close `/provision` — **DONE**

Nothing else on this list matters while forty curl commands can take the platform down.

Shipped: four checks run before anything is created — **global** rate limit → per-source rate
limit → project cap → the artifact must actually exist — plus an hourly reaper. Each has a test
that proves it *blocks*, not that it exists. Details in
[infra/auth/README.md](../infra/auth/README.md#what-guards-provision).

**Deliberately no credential.** The only caller-controlled input is the id; port, database name,
container name and JWT secret are all reused from what the project already has, so re-provisioning
someone else's restarts their container with identical config and changes nothing. A key would
have guarded a takeover the endpoint's shape already makes impossible, while adding a secret to
distribute and a way to lock a real user out of their own project.

**The global limit is what does the work.** Per-source limiting assumes addresses are scarce and
they are not — cloud IPs are rentable by the thousand, and one IPv6 allocation is 18 quintillion
addresses, so against a distributed attacker it is theatre. A limit on what the *host* will do per
hour cannot be escaped by having more addresses. The accepted cost is that a flood can consume the
budget and delay legitimate provisions for the rest of the window; a real user waits, where the
alternative was the box running out of memory.

The one item deferred from the original list is item 5 below — the per-provision `nginx -s reload`
is still there. With `MAX_PROJECTS` capping the config count at 50, `nginx -t` cost is bounded and
the O(N²) term cannot be reached, so the map-file rewrite is no longer urgent.

1. **Require the deploy token.** The same trust-on-first-use secret already designed for the
   hosting plan and already shipped in pattern by `src/db.rs::key_for()`. A caller who cannot
   present the token for an id cannot provision it. This alone removes the anonymous path.
2. **Bind provisioning to a real artifact.** Refuse any `project_id` that does not correspond to
   an artifact actually published from this account. Provisioning auth for a project that does not
   exist is never legitimate.
3. **Hard ceiling on live projects** — a global cap (start at 50) and a per-token cap (2). Refuse
   past it, fail closed.
4. **Rate limit** — per IP and per token, at nginx (`limit_req`) *and* in the control plane.
5. **Stop reloading nginx per provision.** Use a single wildcard `location ~ ^/([a-z2-9]{4,16})/`
   that maps id → port from a map file, so adding a project rewrites one map and sends `HUP`
   rather than re-parsing every conf. Removes the O(N²) term entirely.
6. **Reaper.** Delete containers and databases for projects whose artifact has expired. The 48h
   artifact lifetime already defines when; nothing acts on it today, so provisioned state
   accumulates forever.

### P1 — Put `auth.boxcode.sh` behind CloudFront (a day, ~$0 at this traffic)

Removes the direct-to-IP attack surface entirely.

1. New CloudFront distribution, alias `auth.boxcode.sh`, origin = the box's DNS name, ACM cert.
2. **Cloak the origin.** After cutover the box must not be reachable by IP: restrict the security
   group's 443 to the `CLOUDFRONT_ORIGIN_FACING` managed prefix list, and require a shared secret
   header that nginx checks and rejects without. Otherwise the A record just moves and the old IP
   stays attackable.
3. **AWS WAF** on both distributions:
   - Rate-based rule, 2000 req / 5 min / IP, block.
   - A tighter rate rule scoped to `/provision`, `/db/*`, `/uploads` — 60 req / 5 min / IP.
   - `AWSManagedRulesCommonRuleSet` + `AWSManagedRulesAmazonIpReputationList`.
4. `limit_req` in nginx as the second layer, so a WAF bypass still meets a limit.

### P2 — Bound the Lambda blast radius (hours, ~$0)

1. **Reserved concurrency on `boxcode-artifact-signer`** — 50. This both caps what a flood can
   consume *and* guarantees the signer that much, so the two directions are covered at once.
2. Reserved concurrency 2 per hosted app function, as the hosting plan already specifies.
3. **Never set an account-wide concurrency cap.** With 42 production functions sharing the pool,
   that is an outage, not a control.

### P3 — Detection and the kill switch (a day)

Alarms on the four signals in §6, plus a **tag-scoped** Budgets alarm (`boxcode:hosting`) driving a
kill switch that disables `/api/deploy` and zeroes reserved concurrency **on `boxcode-app-*` only**.
An account-wide budget or an unscoped kill switch would fire on unrelated production spend and take
down 42 functions.

### P4 — Structural, when it is worth it

- **Split the box.** auth, db, requests and uploads on one t3.small means one attack kills all
  four. Separate instances, or move the db relay to its own box, so a flood against auth does not
  take the database with it.
- **Shield Advanced** — $3,000/month plus DDoS Response Team access and cost protection. Not
  justified today. Revisit if boxcode carries revenue-bearing traffic.

---

## 5. Cost

| Control | Monthly |
|---|---|
| Shield Standard (CloudFront + EC2) | **$0** — automatic |
| CloudFront for `auth.boxcode.sh` | **~$0** at current traffic (1 TB free tier) |
| AWS WAF web ACL | $5 per ACL + $1 per rule + $0.60/M requests → **~$10–15** for two ACLs |
| ACM certificate | **$0** |
| CloudWatch alarms | **~$1** |
| P0 (code + config only) | **$0** |
| **Total** | **≈ $15/month** |
| *Shield Advanced, if ever* | *$3,000* |

The controls that matter most — P0 and origin cloaking — cost nothing. This is not a budget
problem.

---

## 6. Detection: the four signals

Alarm on these; everything else is noise.

| Signal | Threshold | Means |
|---|---|---|
| `/provision` calls per hour | > 20 | Someone is walking the id space |
| Live GoTrue container count | > 50 | Provision abuse, or the reaper has stopped |
| Box memory available | < 200 MB | Minutes from OOM |
| Lambda account concurrent executions | > 400 | A flood is eating the shared pool |

Plus t3 CPU credit balance trending to zero — the box does not recover on its own once credits are
gone, so this is an early warning that outlasts the attack.

---

## 7. Runbook — when it is happening

**Confirm what is being hit** (30 seconds)

```bash
aws cloudwatch get-metric-statistics --namespace AWS/CloudFront \
  --metric-name Requests --dimensions Name=DistributionId,Value=E2JMTKNA76TEEX \
  --start-time $(date -u -v-1H +%FT%TZ) --end-time $(date -u +%FT%TZ) \
  --period 300 --statistics Sum
aws ssm start-session --target i-091cf663e3c2d1a94
#   free -m ; docker ps | wc -l ; tail -f /var/log/nginx/access.log
```

**Stop the bleeding, in this order**

1. **Provision abuse** → block `/provision` at nginx (`return 429`) and reload. The agent loses
   `enable_auth`; everything else keeps working. Reversible in seconds.
2. **L7 flood** → drop the WAF rate-based threshold; add an IP-set block rule for the top talkers.
3. **Lambda concurrency starving production** → set reserved concurrency on the signer immediately;
   that caps it without touching the other 41 functions.
4. **Box already down** → stop the four services, `docker rm -f $(docker ps -q --filter
   name=gotrue-)` for containers created during the window, restart. The reaper in P0 makes this
   unnecessary; until then it is manual.
5. **Cost running away** → the tag-scoped kill switch, or disable the `/api/deploy` behavior in
   CloudFront by hand.

**Afterwards** — pull the WAF sampled requests and the audit log, decide bans, and write down which
threshold should have fired earlier.

---

## 8. What we are choosing to accept

Stated plainly so nobody assumes otherwise.

- **A determined volumetric attack against the box wins until P1 ships.** A t3.small on a public IP
  cannot be defended by configuration.
- **Shield Standard does not mean protected.** It is a free L3/L4 floor. It will not save a
  2 GB instance.
- **A first phishing offence is not preventable**, only detectable. Speed of takedown is the lever.
- **All four dynamic services share one box.** Until P4 they share one failure.
- **The database has one day of backup granularity** (`infra/db/backup.sh`, merged in #117). Point-in-time
  recovery does not exist.

---

## 9. Reality check on this document

- §2 is verified against the live account, not inferred. Commands are named so it can be re-run.
- §3.1's arithmetic (~40 containers to exhaust 2 GB) is an **estimate from GoTrue's typical
  resident size**, not a measured figure. The failure mode is certain; the exact number is not.
  **It has not been tested against production, and should not be.**
- Shield Advanced status is **unknown** — IAM denied the read.
- CloudFront's current behaviours and whether a WAF is already attached could not be read either
  (`cloudfront:GetDistribution*` denied). **Confirm in the console before building P1.**

**P0 is done.** Next is P1 — putting `auth.boxcode.sh` behind CloudFront and cloaking the origin,
which is the one structural change that a t3.small on a public IP cannot do without.
