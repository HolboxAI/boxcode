// The last resort. Fired by an SNS message from the tag-scoped budget alarm,
// or by hand.
//
// It stops hosted projects serving and stops the deploy endpoint accepting new
// ones. It deletes nothing: no microVM image, no database, no registry entry.
// Everything it does is reversible in one command, which is the property that
// makes a kill switch one people dare to arm -- one that deleted would be one
// nobody ever fired.
//
// **This used to throttle Lambda functions.** Hosted backends were Lambdas, and
// stopping them meant setting reserved concurrency to zero. They are microVMs on
// a box now, so this sends one SSM command to that box and the work happens
// there, in lifecycle/kill-switch.sh.
//
// The guarantee moved with it, and is worth stating precisely because it is the
// whole reason this function exists rather than a person running a script:
//
//   Then: an IAM policy granting PutFunctionConcurrency on
//         `function:boxcode-app-*` and nothing else, so a bug could not touch
//         the other 42 functions in this shared account.
//
//   Now:  an IAM policy granting ssm:SendCommand on **one instance id** and one
//         document, so a bug cannot reach any other box. Same shape of promise:
//         code you can get wrong, an IAM resource constraint you cannot.
//
// Which VMs get stopped is still decided by scope.mjs, which now runs on the box
// rather than here -- this function has no way to see a process table. That
// module and its sixteen tests are unchanged in intent.

import {
  SSMClient,
  SendCommandCommand,
  GetCommandInvocationCommand,
} from "@aws-sdk/client-ssm";

const ssm = new SSMClient({});

/// The runner box. One instance, set by Terraform, and the same id the IAM
/// policy names -- if these disagree the call is refused by AWS rather than
/// sent somewhere unintended.
const INSTANCE_ID = process.env.RUNNER_INSTANCE_ID;

const SCRIPT = process.env.KILL_SWITCH_SCRIPT
  || "/opt/boxcode-hosting/lifecycle/kill-switch.sh";

/// How long to wait for the box to report back before giving up on the answer.
/// The command itself keeps running: SSM is not cancelled by us losing patience,
/// and the box stopping ten VMs matters more than this function seeing it happen.
const WAIT_MS = 60_000;

async function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

async function runOnBox(action) {
  const sent = await ssm.send(
    new SendCommandCommand({
      InstanceIds: [INSTANCE_ID],
      DocumentName: "AWS-RunShellScript",
      Comment: `boxcode kill switch: ${action}`,
      Parameters: { commands: [`bash ${SCRIPT} ${action}`] },
      TimeoutSeconds: 600,
    }),
  );

  const commandId = sent.Command?.CommandId;
  if (!commandId) return { commandId: null, status: "unknown", output: "" };

  const deadline = Date.now() + WAIT_MS;
  let last = { status: "Pending", output: "" };
  while (Date.now() < deadline) {
    await sleep(3000);
    try {
      const inv = await ssm.send(
        new GetCommandInvocationCommand({ CommandId: commandId, InstanceId: INSTANCE_ID }),
      );
      last = {
        status: inv.Status ?? "unknown",
        // Both streams: the box logs what it stopped and what it left alone on
        // stderr, and that is exactly what someone reads during an incident.
        output: `${inv.StandardOutputContent ?? ""}${inv.StandardErrorContent ?? ""}`.trim(),
      };
      if (!["Pending", "InProgress", "Delayed"].includes(last.status)) break;
    } catch (e) {
      // InvocationDoesNotExist for the first second or two after sending is
      // normal, not a failure.
      if (e.name !== "InvocationDoesNotExist") throw e;
    }
  }
  return { commandId, ...last };
}

export const handler = async (event) => {
  // Two ways in: an SNS notification from the budget alarm, or a direct invoke
  // with {"action":"restore"} to put things back. Restoring is deliberately not
  // automatic -- a budget that dropped back under its threshold because the
  // month rolled over is not evidence the abuse stopped.
  let action = "stop";
  if (event?.action === "restore") action = "restore";
  if (event?.action === "status") action = "status";

  if (!INSTANCE_ID) {
    // Refused rather than guessed at. Sending to the wrong instance, or to
    // every instance, is the failure this whole file is arranged to prevent.
    const out = { action, ok: false, why: "RUNNER_INSTANCE_ID is not set" };
    console.error(JSON.stringify(out));
    return out;
  }

  const result = await runOnBox(action);
  const out = {
    action,
    instance: INSTANCE_ID,
    ok: result.status === "Success",
    status: result.status,
    commandId: result.commandId,
    output: result.output,
  };

  // Logged in full, every time. During an incident the question is "what did it
  // touch", and the answer has to be in the log rather than inferred.
  console.log(JSON.stringify(out));
  return out;
};
