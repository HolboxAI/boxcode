# Hosting

One dedicated **m8i.large** running each hosted project in its own **Firecracker
microVM**, plus the rate limiting and cost containment around them.

> **Substrate history.** Lambda first — it could not run what people build (no
> WebSockets, a 30s cap, no database wire protocol, so every ORM fails). Then EC2
> with containers. Now Firecracker, because containers share a kernel and that
> was the one risk the container design could not close.

## Why this is possible now and was not in January

Firecracker needs KVM, and AWS only exposed KVM on **bare metal**. The cheapest
x86 metal is `c5.metal` at about **$2,978/month**, so renting a physical server
to host ten demo sites was never going to happen.

In **February 2026** AWS added a `NestedVirtualization` CPU option to ordinary
EC2 instances. Verified against the EC2 User Guide, August 2026:

- Supported: C8i, M8i, R8i, C8id, R8id, M8id, the `-flex` variants, X8i, C7i,
  R7i, M7i, I7i. **Graviton is not supported**, so the cheaper ARM families are out.
- *"There is no additional cost for using nested virtualization."*
- **KVM is a supported L1 hypervisor** — which is what Firecracker needs.
- The docs list supported *types*, not sizes, so `.large` qualifies.

That took self-hosting Firecracker from $2,978/month to **$97**.

## Cost

| | |
|---|---|
| **Fixed, cannot drift** | **$93.41** |
| Variable, expected | $3.40 |
| **Total** | **$96.81/mo** — $9.68 per project |

Fixed is `m8i.large` $77.26 + EBS 50 GB $4.00 + Elastic IP $3.65 + WAF ACL $5.00
+ 3 WAF rules $3.00 + 5 alarms $0.50.

Variable is the only part that can move, and all of it: WAF request inspection
($0.60/M), CloudWatch Logs ingest ($0.50/GB), EBS snapshots, S3. Worst realistic
case — ten times the traffic *and* a heavily-logging project *and* a filling disk
— is **$112.71**, which is why `budget_usd` is **110**: above an ordinary month,
below the bad one.

> `budget_usd` was **25** when hosted backends were Lambdas. Left there, the
> tag-filtered budget would fire on day one and every day after, because the
> Firecracker host itself is tagged `boxcode:hosting` and costs $97/month.

## Ten projects, and where that number comes from

A microVM has **its own guest kernel**, so its memory is spent from the host the
moment it boots. Nothing is shared and nothing is reclaimable — which is also why
there is deliberately **no balloon device**, since a ceiling the host can move is
not a ceiling.

| | MiB |
|---|---|
| Host OS and kernel | 250 |
| 10 jailed VMM processes @ ~5 MB | 50 |
| **10 microVMs @ 256 MiB** | **2,560** |
| Postgres | 250 |
| nginx and control plane | 80 |
| rootfs build burst | 512 |
| **Committed** | **3,702 of 8,192 — 45%** |

There is a test asserting this, not a comment, because it is the number the whole
design was costed against.

## How a project is confined

Two boundaries, protecting against different attackers.

**The guest** gets one vCPU, 256 MiB, and its own kernel. `smt: false` — hyperthread
siblings share microarchitectural state, which is the substrate for most cross-VM
side-channel work. Disk and network are rate-limited **by Firecracker itself**, so
a tenant cannot lift them from inside.

**The VMM** is the part worth being careful about. A microVM is a strong boundary,
but the VMM process sits on the *host* side of it — running it as root would turn a
Firecracker vulnerability into host root directly. So every VM is started through
Firecracker's **`jailer`**, which chroots it, drops it to an unprivileged uid, puts
it in a cgroup and a network namespace, and installs a seccomp filter. Each slot
gets **its own uid**: one shared uid would let a VMM that escaped its chroot signal
or ptrace the other fifteen, which is most of what escaping a chroot is good for.

There is also **no API socket** — `--no-api`. The VM is fully described by its
config file at boot, and a live socket is a control channel into the VMM that
nothing here needs.

### Networking, and why guests cannot reach each other or the internet

Each microVM sits on a **point-to-point /30** with the host, inside its own network
namespace. Slot 0 is `10.200.0.0/30`, slot 1 is `10.200.1.0/30`. A guest has no
route to another guest because its netmask covers four addresses — that is
structural, not a firewall rule someone could edit away.

`ip_forward` is set to **0** and asserted, with no NAT anywhere. There is no route
off the box at all: no mining pool, no C2, no spam relay, nowhere to exfiltrate to.

Two constraints are load-bearing and neither is obvious:

- **Linux caps interface names at 15 characters** (IFNAMSIZ is 16 with the NUL).
  Project ids run to 16, so `fc-tap-<id>` does not fit and the device *silently
  fails to create*. Devices are named by slot index instead.
- **The subnet base is `10.200`, not `172.31`** — this account's VPC is
  `172.31.0.0/16`, and a guest network overlapping it would kill the host's route
  to the auth box at `172.31.22.160`, appearing at whichever slot first collided
  rather than on day one.

Both have tests.

## Turning a project into a bootable disk

Firecracker boots a kernel and hands it a block device. Neither exists on its
own — this is the part the Firecracker README means when it says image
management is *"an external concern users must address separately"*.

```
base rootfs (one per runtime, built once at setup)
  + the project's built tree
  + a generated init
  = one ext4 file per project
```

**Built with `mke2fs -d`**, which populates a filesystem image from a directory
*without mounting it*. That matters more than it sounds: the obvious alternative
is a loop device and a real mount, which needs `CAP_SYS_ADMIN` in the host mount
namespace — on the one box where handing that out is least appealing. `mke2fs`
needs neither.

**Alpine, not Amazon Linux.** A minimal Alpine root is about 8 MiB against
roughly 200 for a `dnf --installroot` of AL2023. Every megabyte is paid ten times
over — once per project image on a 50 GiB disk — and again in the seconds a
deploy spends copying it.

### The init is the guest's PID 1

Deliberately not an init system. There is one process to run, nothing to
supervise, and no service ordering. If the app exits, PID 1 exits and the kernel
panics — with `panic=1` in the boot arguments that stops the microVM dead instead
of leaving it holding 256 MiB doing nothing, which is exactly right: the control
plane sees a stopped VM and decides.

It `exec`s rather than forks, so the app *is* PID 1, and drops to uid 1000 via
`su-exec` first. The microVM is the boundary that matters, but a guest kernel
exploit is easier from root and dropping costs nothing.

There is no DHCP client. The kernel configures `eth0` from the `ip=` boot
argument, which is most of why a microVM is serving in a fraction of a second.

### Dependencies are installed inside a microVM too

`npm install` runs arbitrary `postinstall` code. That is true of every CI system
ever built and it is not preventable — containment is the only answer, and it has
to be at least as strong as what the app itself gets, or **the build becomes the
soft way into a platform whose entire premise is per-tenant hardware isolation**.

So `install-deps.sh` boots the project's own image as a microVM, with
`init=/sbin/build-init` on the kernel command line instead of the default. Same
disk, different PID 1 — which is what makes it cheap: dependencies land in `/app`
and are simply there when the app later boots the same disk normally. **Nothing
is copied out, nothing is mounted, and there is no second block device.**

Three differences from an app microVM, and only three:

| | App VM | Build VM |
|---|---|---|
| Network | none | NAT'd, through the build slot |
| Memory | 256 MiB | 1024 MiB — `npm install` peaks high |
| Init | `/sbin/init` | `/sbin/build-init`, then powers itself off |

The result is read back with `debugfs -R cat`, which reads an ext4 image
**without mounting it** — the host must never mount a filesystem a stranger's
build has just finished writing to. The guest writes a start marker before doing
anything and its exit code afterwards, so a VM that never booted is
distinguishable from one whose install failed; without the marker both look
identical from outside, as a missing status file.

A failed build leaves the project exactly as it was. The image is only moved back
over the original on success, so a redeploy retries from a known state rather than
from whatever the last attempt managed before it died.

### Where app TAPs live, and what that costs

App TAPs are in the **host** network namespace. Only the build slot gets a
namespace of its own.

The first version of this put every slot in a namespace, which is wrong in a way
that is easy to miss: a namespace hides the TAP from the host, and it equally
hides the guest from **nginx**, which runs in the host namespace and has to reach
the app to serve it. There was no route in. **Nothing would ever have answered a
request.**

So a guest's limits are two firewall facts rather than a topological one, and
`setup.sh` asserts both rather than assuming them:

| | |
|---|---|
| `ip_forward=0` | A guest reaching another guest, or the internet, would be the host forwarding between two interfaces. It does not. |
| `INPUT` rules | A guest may reach Postgres on this box and nothing else — not sshd, not nginx's own port, not the control plane. |

Each guest is still on a point-to-point /30, so it has no route to anything but
the host end of its own link. This is a weaker *kind* of guarantee than topology
— a rule can be removed where a missing interface cannot — which is precisely why
provisioning fails if either check does not hold.

The build slot keeps its namespace, because it genuinely needs forwarding and NAT
that must not exist anywhere else on the box, and `ip_forward` is namespaced.

## A database per project

The reason this is not on Lambda: a real wire protocol, so Prisma, SQLAlchemy,
the Django ORM and everything else work untouched against a plain
`DATABASE_URL`.

**The isolation rule is not the default, and getting it wrong is invisible:**

> PostgreSQL grants `CONNECT` on **every** database to `PUBLIC`. A role per
> project therefore buys nothing on its own — any project could open any other
> project's database the moment it reached the port.

So each database revokes `CONNECT` from `PUBLIC` and grants it back only to its
own role, and `setup.sh` does the same to `postgres` and **`template1`** — without
the latter, every database created afterwards starts with the hole reopened. The
same default exists one level down on the `public` schema, and is closed the same
way.

Each role is `NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS`,
capped at **5 connections** (ten projects cannot take all 60), with a 10s
`statement_timeout` and a 60s `idle_in_transaction_session_timeout` — without the
latter one project's abandoned transaction holds locks that block its own future
migrations forever.

**Passwords rotate on every deploy** rather than being stored and reused. A
password that never changes outlives the project it belonged to, sitting in an
image and a shell history for as long as anyone keeps either.

SQL identifiers cannot be parameterised, so the anchored id pattern is not a tidy
check — it is the only thing between a project id and arbitrary SQL. There is a
test asserting that nothing but a valid id can reach the generated SQL.

## Starting and stopping

The control plane is **not** what keeps a microVM alive — the jailer processes
are, and they outlive it. So a control-plane restart finds a box with VMs already
running and a registry describing what *should* be running, and those disagree in
every interesting case:

| Registry | Running | Action |
|---|---|---|
| yes | yes | **adopt** — do nothing |
| yes | no | **start** |
| no | yes | **stop** — it is leaked memory |
| expired | either | **reap** |

Getting the third case wrong is how a box slowly fills with 256 MiB allocations
nobody can account for, until the eleventh deploy fails for no visible reason.
Getting it *too* aggressive is worse: stopping something merely unrecognised takes
a live tenant down. So `reconcile.mjs` only ever stops a VM whose **name** says it
is ours — the same anchored rule the kill switch uses — and everything else is
logged as ignored and left running.

`vm.sh list` reads the process table rather than pid files: **a pid file written
by something that then crashed is a lie**. The slot is recovered from the jailer's
uid, which is `30000 + slot`.

The registry survives being damaged. It will eventually be truncated by a power
loss mid-write, or hand-edited during an incident, so `parse` keeps every entry
that is intact, drops the rest **with a reason**, and lets reconciliation deal with
the consequences. A control plane that refuses to start over one malformed entry
takes the whole platform down to protect one project.

## The control plane

One always-running process. Everything it decides lives in `runtime/` as pure,
tested modules; `control-plane/index.mjs` is orchestration, I/O and the HTTP
surface, and is deliberately thin because it is the part with no tests.

**Deploys are asynchronous.** A build takes minutes and CloudFront gives an
origin 60 seconds to respond, so `POST /api/deploy` accepts the work and returns
immediately; the client polls `GET /api/deploy/status/<id>`. A synchronous deploy
would have worked in testing and timed out in production.

```
POST /api/deploy   ->  gate -> capacity -> slot -> registry written
                       then, in the background:
                       database.sh provision  ->  DATABASE_URL
                       rootfs/assemble.sh     ->  ext4 image
                       rootfs/install-deps.sh ->  build microVM
                       lifecycle/vm.sh start  ->  running
```

The registry entry is written **before** the work starts, so a crash mid-deploy
leaves an entry reconciliation will either start or reap — rather than a running
VM nothing claims.

Deploys are serialised, not queued: two builds each want 1 GiB, and the box is
sized for ten projects plus one build.

### The gate

| | | |
|---|---|---|
| A1 | Token bound to the project on first use | An eight-character guess would otherwise replace a running server |
| A2 | 2 live projects per token | |
| A3 | 5 deploys/hour, 20/day per token | |
| A4 | 3 new projects/day per address | |
| A5 | 10 deploys/hour per address | Above A3, because a shared office NAT is one address with several honest people behind it |
| A6 | Token and address blocklist | Refused first, and without explanation |
| A7 | Every deploy recorded with token hash and source | |

**A2 and A4 are a pair.** A cap of two projects per token is defeated by minting
tokens; a cap on new tokens per address is defeated by one token taking every
slot. Together, occupying the platform needs many addresses as well as patience.

Tokens are stored **hashed** and compared in constant time — a registry readable
by anyone who gets a copy of the disk should not hand them the ability to take
over live projects. The gate **fails closed**: an unreadable clock refuses
everything, because every limit is a window against that clock.

### Capacity is measured, not counted

A microVM's memory is spent the moment it boots — its own guest kernel, no page
cache to evict, nothing shared, and deliberately no balloon device. So 256 MiB
per project is 256 MiB of the box, always, idle or not. That is the price of the
isolation, and it is why ten is the number rather than thirty.

Admission reads `MemAvailable` and refuses when accepting would eat the reserves,
so ten idle FastAPI services and six Next.js servers are both correct answers.

## Testing it end to end

Everything that can be checked without KVM already is, in the test suites below.
The rest needs a host with `/dev/kvm`, which means the runner box — macOS cannot
do it and neither can Docker Desktop's Linux VM.

**Testing is cheap because you stop the box afterwards.** `m8i.large` is
$0.10584/hour; three hours is 32 cents. The $97/month figure is 24/7 running.

```bash
# 1. just the box -- not the WAF, budget or SNS, which need permissions you may
#    not have and prove nothing about whether hosting works
cd infra/hosting
terraform apply -target=aws_instance.runner -target=aws_eip.runner \
  -var runner_subnet_id=subnet-... -var runner_vpc_id=vpc-... -var alert_email=you@holbox.ai

# 2. on the box, over plain HTTP -- no DNS, no certificate, no CloudFront
sudo dnf install -y git && git clone <repo> && cd boxcode
SKIP_TLS=1 bash infra/hosting/setup.sh

# 3. the whole pipeline, end to end
bash infra/hosting/smoke-test.sh

# 4. stop it
aws ec2 stop-instances --instance-ids <id>
```

`smoke-test.sh` builds a project that uses its database and reads `PORT`, takes
it through provisioning, assembly, a build microVM and start, then checks it
**serves**, **reaches PostgreSQL**, **cannot reach the internet**, and is
**visible to reconciliation** — then removes it. Nothing is left behind.

The egress check is the one to watch. A failure there means the highest-value
control in the design is not working, and it is the one thing that cannot be
inferred from the unit tests.

Only once that passes is there any point in DNS, a certificate, or CloudFront.

## Testing

```bash
node --test infra/hosting/runtime/network.test.mjs   # 15
node --test infra/hosting/runtime/machine.test.mjs   # 21
node --test infra/hosting/runtime/rootfs.test.mjs    # 21
node --test infra/hosting/runtime/build.test.mjs     # 22
node --test infra/hosting/runtime/registry.test.mjs  # 17
node --test infra/hosting/runtime/reconcile.test.mjs # 16
node --test infra/hosting/runtime/database.test.mjs  # 21
node --test infra/hosting/runtime/gate.test.mjs      # 26
node --test infra/hosting/runtime/capacity.test.mjs  # 11
node --test infra/hosting/kill-switch/scope.test.mjs # 10
```

`setup.sh` also asserts at run time that its own `SLOT_COUNT` and
`SUBNET_PREFIX` agree with `runtime/network.mjs`. They are duplicated — one
creates the devices, the other decides what to look for — and a silent
disagreement shows up as microVMs that boot with no network rather than as
anything resembling a configuration error.

## Applying

Needs **AWS provider >= 6.0**; `nested_virtualization` does not exist in 5.x.
`runner.tf` needs `runner_subnet_id` and `runner_vpc_id`, then
`bash infra/hosting/setup.sh` on the box.

`setup.sh` refuses immediately if `/dev/kvm` is missing, and says which of the two
likely causes it is — nested virtualization off, or an unsupported instance type.
Without that check it surfaces much later as a confusing permissions error on the
first deploy. Nested virtualization can only be changed while the instance is
**stopped**.

Point `apps.boxcode.sh` at the Elastic IP before the certbot step, or use
`SKIP_TLS=1` to prove everything else first.

**No control-plane yet**, so no deploys. `setup.sh` finishes with a clear message
rather than failing, so the box comes up as a working Firecracker host with
nothing driving it.

## The strategy: four layers, cheapest first

Each catches what the one above it misses. Each is cheaper than the next,
because the earlier a request is refused the less it costs to refuse.

```
                    a flood arrives
                          │
  L0  Shield Standard ────┤  L3/L4 volumetric, absorbed at the edge.
      (free, automatic)   │  Free, already on, nothing to configure.
                          ▼
  L1  WAF rate rules  ────┤  One address asking too often is BLOCKED AT THE
      2000/5min per IP    │  EDGE — before the box sees it at all. This is the
      300/5min on /api/*  │  layer that handles "millions of requests" from a
                          │  small set of hosts.
                          ▼
  L2  Per-VM ceilings ────┤  256 MiB, 1 vCPU, rate-limited disk and network,
      (Firecracker)       │  enforced by the VMM where a tenant cannot lift
                          │  them. Caps what any one project can consume.
                          ▼
  L3  Budget alarm    ────┤  Last resort. Tag-filtered at $110 → SNS → kill
      → kill switch       │  switch stops every hosted microVM and makes
                          │  /api/* return 503.
```

**Why layered rather than one big limit.** L1 is per-address and fails against
a distributed attacker. L2 is aggregate and would let one noisy client spend
everyone's budget if it were alone. L3 bounds cost but not request count. L4
catches whatever gets through all three. No single one of them is sufficient,
and each is much cheaper than the one below it.

## Why this cannot take down other services

Three code checks, and one thing that is not code.

**The code** (`kill-switch/scope.mjs`) requires all three to agree:

1. Name matches `^boxcode-app-[a-z2-9]{4,16}$` — anchored, so
   `boxcode-app-x-prod` and `not-boxcode-app-x` do not match
2. The name is not on the never-touch list (the signer, deploy-control, the
   reaper, the kill switch itself)

**The guarantee** is the IAM policy in `guards.tf`. It grants `ssm:SendCommand`
on **one instance id** and **one document** — so even a bug in `scope.mjs`
cannot reach another box, because AWS refuses the call before it runs. Code you
can get wrong; an IAM resource constraint you cannot.

The same technique verified the Lambda-era version of this policy against the
real account, and is worth repeating whenever the resource changes:

```bash
aws iam simulate-custom-policy \
  --policy-input-list "$(terraform show -json | jq -r '...')" \
  --action-names ssm:SendCommand \
  --resource-arns arn:aws:ec2:us-east-1:992382417943:instance/<some-other-instance> \
  --query 'EvaluationResults[0].EvalDecision' --output text
# expect: implicitDeny
```

## Two things deliberately absent

**No account-level Lambda concurrency cap.** It would throttle all 42 functions
in this account into one shared pool — a company-wide outage dressed as a cost
control. Nothing boxcode hosts runs on Lambda any more, which makes this cheaper
to honour rather than less important: a control that cannot affect another
service by construction is better than one scoped carefully.

**No account-wide budget.** Existing spend is orders of magnitude above any
threshold useful for boxcode, so an account budget would fire immediately,
permanently, and be muted within a week. The budget here filters on the
`boxcode:hosting` cost-allocation tag and measures only tagged resources.

> The tag must be **activated** in Billing → Cost allocation tags before the
> budget can see it, and activation is not retroactive.

## Alarms

| Alarm | Threshold | Means |
|---|---|---|
| `boxcode-runner-status-check` | 2 min failing | The box is gone or wedged. **Every hosted project is down** and nothing else will say so. Treats missing data as breaching — an instance that stopped reporting is at least as concerning as one reporting a failure. |
| `boxcode-runner-disk` | >80% | A full disk stops every microVM and Postgres together, is caused by ordinary use, and is the outage this box will actually have. Needs the CloudWatch agent; without it this sits in INSUFFICIENT_DATA, which is itself worth noticing. |
| `boxcode-account-concurrency-high` | >400 for 10 min | Watches the *other* 42 Lambda functions in this shared account, not ours. Still valid, and still **watch only — never cap this account-wide**. |
| Budget 60% | $66 | Warning, email only — look before it acts |
| Budget 100% | $110 | SNS → kill switch |

`boxcode-app-throttles` was removed with the Lambda substrate. There is no
`Throttles` metric for a microVM, and an alarm that can never fire is worse than
no alarm — it reads as coverage.

## Applying the guards

`terraform apply` needs credentials able to create IAM roles, a Lambda, an SNS
topic, a budget and a WAF ACL.

Order matters: **activate the `boxcode:hosting` cost-allocation tag first**, in
Billing → Cost allocation tags, or the budget measures nothing. Activation is not
retroactive.

## The kill switch

Fired by SNS from the tag-scoped budget alarm, or by hand.

```bash
aws lambda invoke --function-name boxcode-kill-switch --payload '{}' /dev/stdout
aws lambda invoke --function-name boxcode-kill-switch --payload '{"action":"restore"}' /dev/stdout
aws lambda invoke --function-name boxcode-kill-switch --payload '{"action":"status"}' /dev/stdout
```

It stops projects **serving**. It deletes nothing — no image, no database, no
registry entry. Reversible in one command, which is the property that makes a
kill switch one people dare to arm; one that deleted would be one nobody fired.

**It used to throttle Lambda functions**, from when hosted backends were
Lambdas. It now sends one SSM command to the runner and the work happens there,
in `lifecycle/kill-switch.sh`. The guarantee moved with it:

| | Resource the IAM policy names |
|---|---|
| Then | `lambda:PutFunctionConcurrency` on `function:boxcode-app-*` |
| Now | `ssm:SendCommand` on **one instance id** and **one document** |

Same shape of promise — a bug in the code cannot reach another box, because AWS
refuses the call before the code runs.

### Why a flag file, and not just stopping things

Stopping ten VMs is not enough on its own. Reconciliation would see ten registry
entries with nothing running and helpfully start them all again within fifteen
minutes — **the switch would undo itself**. So `stop` writes
`state/killed` first, and the control plane starts nothing while it exists.

Stopping and reaping still happen. The switch is about not *serving*, not about
not tidying up.

`restore` removes the flag and starts nothing itself: reconciliation already
knows what should be running, and starting from two places is how a box ends up
with two VMs for one project.

### What decides which VMs get stopped

`kill-switch/scope.mjs`, running on the box — this Lambda has no way to see a
process table. Two checks: the name matches `^boxcode-app-[a-z2-9]{4,16}$`, and
it is not on the never-touch list.

**The tag check is gone**, and that is deliberate rather than an oversight. It
existed because Lambda functions shared an account with 42 others, and a stray
name match could have throttled `gpurouter-agent`. A microVM has no AWS tags;
requiring one would mean the switch could never stop anything, and the failure
would only appear during the incident it exists for. What replaced it is the IAM
scoping above.

The module still refuses every real production function name in this account —
`gpurouter-agent-agent`, `fsi-genai-workshop-*`, `mach11-registration-*` — and
those assertions stay in the test suite, because those names must never match
whatever this switch is pointed at next.

