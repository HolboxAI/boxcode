// boxcode uploads control-plane -- lets a signed-in visitor on a published
// page upload an image, without this box ever seeing the file's bytes.
//
// Zero npm dependencies, same stance as the auth/db/requests control-planes:
// this hand-rolls AWS SigV4 presigned-URL signing with node:crypto rather
// than vendoring the AWS SDK for one call shape. `POST /uploads` verifies
// the caller is a real signed-in user of that project (same GoTrue check
// db_query's access_token support already does, reused verbatim -- see
// verifyUser below) and, if so, hands back a short-lived presigned S3 `PUT`
// URL plus the public URL that upload will be reachable at once done. The
// browser PUTs the bytes straight to S3; they never pass through this
// process or this box.
//
// Credentials for signing come from this instance's own IAM role via IMDSv2
// (see fetchInstanceCredentials) -- never a static key stored on disk. The
// role (see infra/uploads/README.md) can do nothing but `s3:PutObject`
// under `boxcode-artifacts/uploads/*`, so the worst outcome of this
// process being compromised is writes into that one prefix, not the bucket
// at large.
import { createServer } from "node:http";
import { createHmac, createHash, randomUUID } from "node:crypto";

const PORT = Number(process.env.PORT || 8083);
const AUTH_BASE = process.env.AUTH_BASE || "https://auth.boxcode.sh";
const BUCKET = process.env.BUCKET || "boxcode-artifacts";
const REGION = process.env.REGION || "us-east-1";
const PUBLIC_BASE = process.env.PUBLIC_BASE || "https://boxcode.sh";
// How long the presigned URL is valid for -- long enough for a slow mobile
// upload to start, short enough that a leaked URL (e.g. in a browser
// history or a proxy log) is useless soon after.
const URL_EXPIRY_SECONDS = 300;

const PROJECT_ID_RE = /^[a-z2-9]{4,16}$/;
const MAX_UPLOAD_BYTES = 5 * 1024 * 1024;
// Matches artifacts.rs's own PUBLISHABLE image extensions -- an upload is
// just a differently-sourced asset on the same site, so it should not
// accept a format publish_artifact itself would refuse.
const CONTENT_TYPE_EXT = {
  "image/png": "png",
  "image/jpeg": "jpg",
  "image/gif": "gif",
  "image/webp": "webp",
};

function fail(res, code, message) {
  res.writeHead(code, { "content-type": "application/json" });
  res.end(JSON.stringify({ error: message }));
}

class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

// Identical check to infra/db/control-plane/index.mjs's verifyUser --
// GoTrue's own `/user` endpoint is the one already-correct place to ask
// "does this token identify a real, current session", and every control-
// plane on this box that needs it asks the same way rather than each
// growing its own slightly different version.
async function verifyUser(projectId, accessToken) {
  const url = `${AUTH_BASE}/${projectId}/user`;
  let response;
  try {
    response = await fetch(url, { headers: { authorization: `Bearer ${accessToken}` } });
  } catch (e) {
    throw new HttpError(401, `could not verify access_token: ${e.message}`);
  }
  if (!response.ok) {
    throw new HttpError(401, "access_token is invalid or expired");
  }
  const user = await response.json().catch(() => null);
  if (!user || typeof user.id !== "string" || user.id === "") {
    throw new HttpError(401, "access_token verified but returned no user id");
  }
  return user.id;
}

// IMDSv2: a token first (the hop that blocks the classic SSRF-via-IMDSv1
// path), then the role's temporary credentials. The role name itself is
// fetched rather than hardcoded -- one instance may only ever have one
// role attached, but nothing here needs to assume which one.
//
// Cached rather than fetched fresh every request: IMDS-issued credentials
// are valid for hours, and re-fetching them per upload would be a wholly
// unnecessary round trip on the hot path. Refetched once within five
// minutes of the expiry IMDS itself reports, rather than on a fixed timer,
// so a slow box under load never signs with a set that expired moments ago.
let cachedCredentials = null;
let cachedExpiry = 0;
async function instanceCredentials() {
  if (cachedCredentials && Date.now() < cachedExpiry - 5 * 60 * 1000) {
    return cachedCredentials;
  }
  const tokenRes = await fetch("http://169.254.169.254/latest/api/token", {
    method: "PUT",
    headers: { "x-aws-ec2-metadata-token-ttl-seconds": "21600" },
  });
  if (!tokenRes.ok) {
    throw new Error(`IMDS token request failed: ${tokenRes.status}`);
  }
  const token = await tokenRes.text();

  const roleRes = await fetch(
    "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
    { headers: { "x-aws-ec2-metadata-token": token } },
  );
  if (!roleRes.ok) {
    throw new Error(`IMDS role lookup failed: ${roleRes.status}`);
  }
  const role = (await roleRes.text()).trim();

  const credRes = await fetch(
    `http://169.254.169.254/latest/meta-data/iam/security-credentials/${role}`,
    { headers: { "x-aws-ec2-metadata-token": token } },
  );
  if (!credRes.ok) {
    throw new Error(`IMDS credentials fetch failed: ${credRes.status}`);
  }
  const creds = await credRes.json();
  cachedCredentials = {
    accessKeyId: creds.AccessKeyId,
    secretAccessKey: creds.SecretAccessKey,
    sessionToken: creds.Token,
  };
  cachedExpiry = new Date(creds.Expiration).getTime();
  return cachedCredentials;
}

function hmac(key, data) {
  return createHmac("sha256", key).update(data, "utf8").digest();
}

function sha256Hex(data) {
  return createHash("sha256").update(data, "utf8").digest("hex");
}

function amzDate(date) {
  return date.toISOString().replace(/[:-]|\.\d{3}/g, "");
}

// AWS SigV4 presigned-URL (query-string) signing, PUT only, for one S3
// object -- narrow on purpose rather than a general-purpose signer, since
// this is the only request shape this process ever needs to produce.
// `content-length` and `content-type` are both in SignedHeaders, not just
// `host`: that is what makes the presigned URL only usable for a PUT
// whose body is exactly the size and type declared when it was requested,
// not merely a URL guarding *where* something can be written with no
// control over *what*.
function presignPut({ credentials, region, bucket, key, contentType, contentLength }) {
  const now = new Date();
  const date = amzDate(now);
  const dateStamp = date.slice(0, 8);
  const host = `${bucket}.s3.${region}.amazonaws.com`;
  const credentialScope = `${dateStamp}/${region}/s3/aws4_request`;
  const canonicalUri = `/${key.split("/").map(encodeURIComponent).join("/")}`;

  const queryParams = {
    "X-Amz-Algorithm": "AWS4-HMAC-SHA256",
    "X-Amz-Credential": `${credentials.accessKeyId}/${credentialScope}`,
    "X-Amz-Date": date,
    "X-Amz-Expires": String(URL_EXPIRY_SECONDS),
    "X-Amz-SignedHeaders": "content-length;content-type;host",
    ...(credentials.sessionToken ? { "X-Amz-Security-Token": credentials.sessionToken } : {}),
  };
  const canonicalQuery = Object.keys(queryParams)
    .sort()
    .map((k) => `${encodeURIComponent(k)}=${encodeURIComponent(queryParams[k])}`)
    .join("&");

  const canonicalHeaders =
    `content-length:${contentLength}\n` + `content-type:${contentType}\n` + `host:${host}\n`;
  const signedHeaders = "content-length;content-type;host";

  const canonicalRequest = [
    "PUT",
    canonicalUri,
    canonicalQuery,
    canonicalHeaders,
    signedHeaders,
    "UNSIGNED-PAYLOAD",
  ].join("\n");

  const stringToSign = [
    "AWS4-HMAC-SHA256",
    date,
    credentialScope,
    sha256Hex(canonicalRequest),
  ].join("\n");

  const kDate = hmac(`AWS4${credentials.secretAccessKey}`, dateStamp);
  const kRegion = hmac(kDate, region);
  const kService = hmac(kRegion, "s3");
  const kSigning = hmac(kService, "aws4_request");
  const signature = createHmac("sha256", kSigning).update(stringToSign, "utf8").digest("hex");

  return `https://${host}${canonicalUri}?${canonicalQuery}&X-Amz-Signature=${signature}`;
}

const server = createServer(async (req, res) => {
  if (req.method !== "POST" || req.url !== "/uploads") {
    return fail(res, 404, "POST /uploads only");
  }

  let body = "";
  for await (const chunk of req) body += chunk;

  let parsed;
  try {
    parsed = JSON.parse(body || "{}");
  } catch {
    return fail(res, 400, "body is not JSON");
  }

  const { project_id: projectId, access_token: accessToken, content_type: contentType } = parsed;
  const contentLength = Number(parsed.content_length);

  if (typeof projectId !== "string" || !PROJECT_ID_RE.test(projectId)) {
    return fail(res, 400, "project_id must look like a boxcode artifact id");
  }
  // Unlike db_query, an access_token is not optional here -- uploading
  // requires a signed-in visitor, full stop (see tools.rs's ENABLE_AUTH
  // description, which is the only place this endpoint is documented for
  // the model). There is no anonymous upload path to fall back to.
  if (typeof accessToken !== "string" || accessToken === "") {
    return fail(res, 400, "access_token is required -- uploading requires a signed-in visitor");
  }
  const ext = CONTENT_TYPE_EXT[contentType];
  if (!ext) {
    return fail(
      res,
      400,
      `content_type must be one of: ${Object.keys(CONTENT_TYPE_EXT).join(", ")}`,
    );
  }
  if (!Number.isInteger(contentLength) || contentLength <= 0) {
    return fail(res, 400, "content_length must be a positive integer, the file's exact byte size");
  }
  if (contentLength > MAX_UPLOAD_BYTES) {
    return fail(res, 400, `content_length exceeds the ${MAX_UPLOAD_BYTES}-byte limit`);
  }

  try {
    const userId = await verifyUser(projectId, accessToken);
    const credentials = await instanceCredentials();
    const key = `uploads/${projectId}/${userId}/${randomUUID()}.${ext}`;
    const putUrl = presignPut({
      credentials,
      region: REGION,
      bucket: BUCKET,
      key,
      contentType,
      contentLength,
    });
    const publicUrl = `${PUBLIC_BASE}/${key}`;
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ put_url: putUrl, public_url: publicUrl }));
  } catch (e) {
    if (e instanceof HttpError) {
      return fail(res, e.status, e.message);
    }
    console.error("uploads /uploads failed:", e);
    fail(res, 500, "could not prepare an upload URL");
  }
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`boxcode uploads control-plane listening on 127.0.0.1:${PORT}`);
});
