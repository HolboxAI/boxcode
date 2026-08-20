# Hosting guards

Rate limiting and cost containment for boxcode's hosted backends.

**Constraint that shapes everything here:** this AWS account is shared. It runs
**42 Lambda functions** and **8 EC2 instances** that have nothing to do with
boxcode — `gpurouter-agent`, `fsi-genai-workshop-*`, `mach11-registration-*`,
`bedrock-gateway-UI-prod`, `gpu-router-prod`. A guard that fired during an
incident and hit those would turn a boxcode cost problem into a company-wide
outage. **That is a worse failure than the one it prevents.**

So every control is scoped to boxcode's own resources, and none of it is
account-wide.

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
      2000/5min per IP    │  EDGE — before API Gateway, before Lambda. A
      300/5min on /api/*  │  blocked request costs $0.60/M instead of an
                          │  invocation. This is the layer that handles
                          │  "millions of requests" from a small set of hosts.
                          ▼
  L2  API Gateway     ────┤  Hard ceiling on invocations per second across
      throttle            │  ALL apps: 200 rps steady, 400 burst. Costs
      (free)              │  nothing. This is what stops a DISTRIBUTED flood
                          │  where every individual address stays under L1.
                          ▼
  L3  Reserved        ────┤  2 concurrent executions per app. Requests past
      concurrency         │  it are throttled by Lambda itself at ZERO compute
      (per function)      │  cost. Caps what any one app can ever spend.
                          ▼
  L4  Budget alarm    ────┤  Last resort. Tag-filtered at $25 → SNS → kill
      → kill switch       │  switch sets reserved concurrency to 0 on
                          │  boxcode-app-* only.
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
2. The function carries the `boxcode:hosting` tag
3. The name is not on the never-touch list (the signer, deploy-control, the
   reaper, the kill switch itself)

**The guarantee** is the IAM policy in `guards.tf`. It grants
`lambda:PutFunctionConcurrency` on `function:boxcode-app-*` **and nothing
else** — so even a bug in `scope.mjs` cannot touch another service, because
AWS refuses the call before it runs. Code you can get wrong; an IAM resource
constraint you cannot.

Verified with IAM's own evaluation engine against the real account:

```
lambda:PutFunctionConcurrency on ...

  boxcode-app-k9depef6                     allowed
  boxcode-app-abcdefgh                     allowed
  gpurouter-agent-agent                    implicitDeny
  fsi-genai-workshop-document-processor    implicitDeny
  mach11-registration-6e237ba2-...         implicitDeny
  boxcode-artifact-signer                  implicitDeny
  holbox-demo-start-builds                 implicitDeny
  maketplacemailing                        implicitDeny
```

Reproduce it:

```bash
POL='{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["lambda:PutFunctionConcurrency"],"Resource":"arn:aws:lambda:us-east-1:992382417943:function:boxcode-app-*"}]}'
aws iam simulate-custom-policy --policy-input-list "$POL" \
  --action-names lambda:PutFunctionConcurrency \
  --resource-arns arn:aws:lambda:us-east-1:992382417943:function:gpurouter-agent-agent \
  --query 'EvaluationResults[0].EvalDecision' --output text
```

## Two things deliberately absent

**No account-level Lambda concurrency cap.** It would throttle all 42 functions
into one shared pool — a company-wide outage dressed as a cost control. Per-
function reserved concurrency is safe by contrast: 10 apps × 2 = 20 drawn from
a pool of 1000, leaving 980 for everything else. And when the kill switch sets
an app to 0, that app's 2 go *back* to the shared pool, so firing it makes more
capacity available to other services, not less.

**No account-wide budget.** Existing spend is orders of magnitude above any
threshold useful for boxcode, so an account budget would fire immediately,
permanently, and be muted within a week. The budget here filters on the
`boxcode:hosting` cost-allocation tag and measures only tagged resources.

> The tag must be **activated** in Billing → Cost allocation tags before the
> budget can see it, and activation is not retroactive.

## The kill switch

Fired by SNS from the budget alarm, or by hand.

It **stops spend; it deletes nothing.** No function, no database, no artifact.
Reserved concurrency going to zero is instantly reversible, so a false alarm
costs an outage of boxcode's hosted apps and nothing more. A kill switch that
deleted would be one nobody dared arm.

```bash
# stop
aws lambda invoke --function-name boxcode-kill-switch \
  --payload '{}' /dev/stdout

# put back
aws lambda invoke --function-name boxcode-kill-switch \
  --payload '{"action":"restore"}' /dev/stdout
```

Restoring is deliberately **not** automatic. A budget dropping back under its
threshold because the month rolled over is not evidence the attack stopped.

Every run logs what it touched *and what it left alone, with the reason* —
during an incident "why is that one still running" is asked at speed, and the
answer needs to be in the log rather than inferred.

## Alarms

| Alarm | Threshold | Means |
|---|---|---|
| `boxcode-account-concurrency-high` | >400 for 10 min | boxcode may be eating the pool the other 42 functions share. **Watch only — never cap this account-wide.** |
| `boxcode-app-throttles` | >100 per 5 min | Reserved concurrency is doing its job, but something is pushing hard |
| Budget 60% | $15 | Warning, email only — look before it acts |
| Budget 100% | $25 | SNS → kill switch |

## Testing

```bash
node --test infra/hosting/kill-switch/scope.test.mjs
```

Ten tests, run against the **real names of the 14 production functions in this
account** rather than invented ones. The important assertion is that not one of
them is touchable — including when they carry the `boxcode:hosting` tag, since
a tag is metadata anyone with Lambda write access could add and the name check
has to stand on its own.

```bash
cd infra/hosting
terraform init -backend=false && terraform validate && terraform fmt -check
```

## Applying

Not applied yet. `terraform apply` needs credentials with permission to create
IAM roles, a Lambda, an SNS topic, a budget and a WAF ACL.

Order matters: **activate the cost-allocation tag first**, or the budget
measures nothing.

L2 (API Gateway throttling) is commented out in `guards.tf` — it attaches to
the HTTP API that the hosting stack creates, which does not exist yet.
