// The last resort. Fired by an SNS message from the tag-scoped budget alarm
// (or by hand), it stops boxcode's hosted backends from serving and stops the
// deploy endpoint from accepting new ones.
//
// It stops spend. It deletes nothing: no function, no database, no artifact.
// Reserved concurrency going to zero is instantly reversible, so a false alarm
// costs an outage of boxcode's hosted apps and nothing else. A kill switch
// that deleted would be one nobody dared arm.
//
// Everything about which functions it may touch lives in scope.mjs, and the
// IAM policy on this function's own role is what makes the answer binding --
// see that file. This one only orchestrates.

import {
  LambdaClient,
  ListFunctionsCommand,
  ListTagsCommand,
  PutFunctionConcurrencyCommand,
  DeleteFunctionConcurrencyCommand,
} from "@aws-sdk/client-lambda";
import { partition } from "./scope.mjs";

const lambda = new LambdaClient({});

// Zero means "accept no invocations". Restoring puts each function back to the
// per-app ceiling the hosting design gives it, rather than to unreserved --
// unreserved would let one app draw on the pool the other 42 functions in this
// account share.
const APP_CONCURRENCY = Number(process.env.APP_CONCURRENCY || 2);

async function listHostedFunctions() {
  const out = [];
  let marker;
  do {
    const page = await lambda.send(new ListFunctionsCommand({ Marker: marker }));
    for (const fn of page.Functions || []) {
      // Tags come from a second call, so only ask for the ones whose name
      // could possibly qualify -- otherwise an incident response spends 42
      // API calls learning what it already knew from the name.
      if (!fn.FunctionName?.startsWith("boxcode-app-")) {
        out.push({ FunctionName: fn.FunctionName, Tags: {} });
        continue;
      }
      const { Tags } = await lambda.send(
        new ListTagsCommand({ Resource: fn.FunctionArn })
      );
      out.push({ FunctionName: fn.FunctionName, Tags: Tags || {} });
    }
    marker = page.NextMarker;
  } while (marker);
  return out;
}

async function apply(action) {
  const functions = await listHostedFunctions();
  const { allowed, skipped } = partition(functions);

  // Logged in full, every time. During an incident the question is "what did
  // it touch", and the answer has to be in the log rather than inferred.
  console.log(
    JSON.stringify({
      action,
      willTouch: allowed,
      leftAlone: skipped.length,
      leftAloneDetail: skipped,
    })
  );

  const results = [];
  for (const name of allowed) {
    try {
      if (action === "stop") {
        await lambda.send(
          new PutFunctionConcurrencyCommand({
            FunctionName: name,
            ReservedConcurrentExecutions: 0,
          })
        );
      } else {
        await lambda.send(
          new PutFunctionConcurrencyCommand({
            FunctionName: name,
            ReservedConcurrentExecutions: APP_CONCURRENCY,
          })
        );
      }
      results.push({ name, ok: true });
    } catch (e) {
      // One failure must not stop the rest. A function that would not throttle
      // is worth reporting, but the other nine still need stopping.
      //
      // An AccessDenied here is the IAM policy doing its job on something that
      // slipped past scope.mjs, which is exactly the belt-and-braces working
      // and should be read as such rather than as a bug.
      console.error(`${action} ${name} failed: ${e.name}: ${e.message}`);
      results.push({ name, ok: false, error: e.name });
    }
  }
  return results;
}

export const handler = async (event) => {
  // Two ways in: an SNS notification from the budget alarm, or a direct invoke
  // with {"action":"restore"} to put things back. Restoring is deliberately
  // not automatic -- a budget that dropped back under its threshold because
  // the month rolled over is not evidence the attack stopped.
  let action = "stop";
  if (event?.action === "restore") action = "restore";

  const results = await apply(action);
  const failed = results.filter((r) => !r.ok);

  return {
    action,
    touched: results.length,
    failed: failed.length,
    detail: results,
  };
};
