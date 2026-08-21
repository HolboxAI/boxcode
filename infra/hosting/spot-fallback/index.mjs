// Keeps the runner box running when spot capacity runs out, and puts it back
// on spot when the shortage passes.
//
// Two ways in, both from EventBridge:
//
//   `EC2 Instance Launch Unsuccessful` from the ASG -- for a spot-only group
//   this means no capacity, and the platform is down or about to be.
//
//   an hourly schedule -- the only thing that ever moves the preference back
//   toward spot.
//
// All of the deciding lives in policy.mjs and is pure; this file reads state,
// calls it, and writes the one field back. The IAM policy on this function's
// role grants autoscaling:UpdateAutoScalingGroup on the runner group's ARN and
// nothing else, so even a bug here cannot reconfigure another group in this
// shared account -- the same belt-and-braces split the kill switch uses.

import {
  AutoScalingClient,
  DescribeAutoScalingGroupsCommand,
  UpdateAutoScalingGroupCommand,
  CreateOrUpdateTagsCommand,
} from "@aws-sdk/client-auto-scaling";
import { ASG_NAME, PERCENT_FOR, mayTouch, decide } from "./policy.mjs";

const asg = new AutoScalingClient({});

/// When the preference last moved. An ASG does not record this, and the
/// decision needs it, so it is kept as a tag on the group itself -- no extra
/// store to provision, back up, or have go missing at the moment it matters.
const SINCE_TAG = "boxcode:fallback-since";

/// Scheduled events carry no ASG detail; ASG events do. Anything else is
/// unrecognised and does nothing rather than guessing.
function triggerFor(event) {
  if (event?.["detail-type"] === "Scheduled Event") return "periodic";
  if (event?.["detail-type"] === "EC2 Instance Launch Unsuccessful") return "launch-failed";
  if (event?.trigger === "periodic" || event?.trigger === "launch-failed") {
    return event.trigger; // direct invoke, for drills and manual recovery
  }
  return null;
}

async function readGroup() {
  const out = await asg.send(
    new DescribeAutoScalingGroupsCommand({ AutoScalingGroupNames: [ASG_NAME] })
  );
  const g = out.AutoScalingGroups?.[0];
  if (!g) return null;

  const tags = {};
  for (const t of g.Tags || []) tags[t.Key] = t.Value;

  // Absent means the group has no mixed-instances policy at all, which is not
  // the shape this function understands. Left undefined so policy.mjs treats it
  // as unreadable rather than as a real zero.
  const pct =
    g.MixedInstancesPolicy?.InstancesDistribution?.OnDemandPercentageAboveBaseCapacity;

  let minutesSinceChange;
  const since = Date.parse(tags[SINCE_TAG] ?? "");
  if (Number.isFinite(since)) minutesSinceChange = (Date.now() - since) / 60000;

  return {
    tags,
    onDemandPercent: pct,
    desired: g.DesiredCapacity,
    inService: (g.Instances || []).filter(
      (i) => i.LifecycleState === "InService" && i.HealthStatus === "Healthy"
    ).length,
    minutesSinceChange,
  };
}

export const handler = async (event) => {
  const trigger = triggerFor(event);
  const group = await readGroup();

  if (!group) {
    // Not an error worth throwing on: the hourly sweep runs forever, and it
    // running before the group exists is an ordinary state during rollout.
    const out = { trigger, action: "hold", why: `no auto scaling group named ${ASG_NAME}` };
    console.log(JSON.stringify(out));
    return out;
  }

  if (!mayTouch(ASG_NAME, group.tags)) {
    const out = {
      trigger,
      action: "hold",
      why: `${ASG_NAME} is missing the required tag; refusing to reconfigure it`,
    };
    console.log(JSON.stringify(out));
    return out;
  }

  const { action, why } = decide({ trigger, ...group });

  // Logged in full every time, decision included. During an incident the
  // question is "why is it on the expensive one" or "why is it still down",
  // and both answers need to be in the log rather than reconstructed.
  const out = {
    trigger,
    action,
    why,
    state: {
      onDemandPercent: group.onDemandPercent,
      desired: group.desired,
      inService: group.inService,
      minutesSinceChange:
        group.minutesSinceChange === undefined ? null : Math.floor(group.minutesSinceChange),
    },
  };

  if (action === "hold") {
    console.log(JSON.stringify(out));
    return out;
  }

  const percent = PERCENT_FOR[action];
  await asg.send(
    new UpdateAutoScalingGroupCommand({
      AutoScalingGroupName: ASG_NAME,
      MixedInstancesPolicy: {
        InstancesDistribution: {
          OnDemandBaseCapacity: 0,
          OnDemandPercentageAboveBaseCapacity: percent,
        },
      },
    })
  );

  // Stamped after the update, not before: a tag claiming a change that failed
  // would start the six-hour clock on something that never happened, and the
  // next sweep would return to spot off the back of it.
  await asg.send(
    new CreateOrUpdateTagsCommand({
      Tags: [
        {
          ResourceId: ASG_NAME,
          ResourceType: "auto-scaling-group",
          Key: SINCE_TAG,
          Value: new Date().toISOString(),
          PropagateAtLaunch: false,
        },
      ],
    })
  );

  out.appliedPercent = percent;
  console.log(JSON.stringify(out));
  return out;
};
