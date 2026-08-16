# boxcode uploads control-plane

Lets a signed-in visitor on a published boxcode page upload an image --
a profile picture, a review photo, a product photo -- without leaving
boxcode to stand up S3/Cloudinary/etc. by hand, and without this box ever
seeing the file's bytes.

Reuses `auth.boxcode.sh`'s existing vhost and cert for the one call it
serves (`POST /uploads`), same as `infra/db/` and `infra/requests/` --
no new DNS or cert needed. Zero npm dependencies, same stance as the
other control-planes: this hand-rolls AWS SigV4 presigned-URL signing
with `node:crypto` rather than vendoring the AWS SDK for one request
shape, and runs fine on the box's system node (no `node:sqlite`, so no
Node 22.5+ requirement).

## The shape: sign, don't relay

`POST /uploads {project_id, access_token, content_type, content_length}`:

1. Verifies `access_token` against that project's own GoTrue (`GET
   {auth_url}/user`) -- the exact same check `db_query`'s `access_token`
   support already does (see `infra/db/`), copied verbatim rather than
   shared, since these are two separate small processes with no shared
   code today. **Required, not optional**: uploading always needs a
   signed-in visitor, there is no anonymous path. A bad or missing token
   is rejected before anything else happens.
2. Checks `content_type` is one of `image/png`, `image/jpeg`,
   `image/gif`, `image/webp` (matching `artifacts.rs`'s own
   `PUBLISHABLE` image extensions -- an upload is just a differently
   sourced asset on the same site) and `content_length` is a positive
   integer under 5MB.
3. Signs a presigned S3 `PUT` URL for
   `boxcode-artifacts/uploads/<project_id>/<verified_user_id>/<uuid>.<ext>`,
   valid for 5 minutes, with `content-length` and `content-type` both in
   `SignedHeaders` -- not just `host`. That is what makes the URL only
   usable for a PUT whose body is exactly the size and type declared when
   it was requested: sending a different size or type fails S3's own
   signature check (`403 SignatureDoesNotMatch`), confirmed live before
   this ever ran on the box (see below). A plain "URL only guards
   *where* something can be written" presigned PUT would not give you
   this for free.
4. Returns `{put_url, public_url}`. The browser `PUT`s the file straight
   to `put_url` -- the bytes never pass through this process or this
   box -- and `public_url` (served through boxcode.sh's own CloudFront,
   see below) is what the page uses from then on.

This process holds no state at all: no registry, no data directory,
nothing on local disk. Every request is independent.

## Where the signing credentials come from

The EC2 instance's own IAM role (`boxcode-auth-ec2-ssm`), via IMDSv2 --
never a static key stored in a file or an environment variable. That
role carries one narrow inline policy added for this feature alone:

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Sid": "AllowUploadsPrefixWriteOnly",
    "Effect": "Allow",
    "Action": "s3:PutObject",
    "Resource": "arn:aws:s3:::boxcode-artifacts/uploads/*"
  }]
}
```

`s3:PutObject` only, `uploads/*` only -- not the bucket at large, and not
even read access to that prefix. The worst outcome of this process being
compromised is writes into that one prefix, nothing else. Credentials are
cached in memory between requests (IMDS-issued ones last hours) and
refreshed once within five minutes of the expiry IMDS itself reports.

## Where the public read side lives (not in this repo)

Unlike `infra/auth/`/`infra/db/`/`infra/requests/`, the public-facing half
of this feature isn't a route this repo's `setup.sh` writes -- it's two
changes made directly against the `boxcode.sh` CloudFront distribution and
its S3 bucket policy, by hand, once:

- A new CloudFront cache behavior, `/uploads/*` -> the existing `s3`
  origin (`boxcode-artifacts`), `GET`/`HEAD` only, the same cache policy
  the existing `/artifacts/*` behavior uses.
- A matching bucket-policy statement letting that CloudFront
  distribution `s3:GetObject` under `uploads/*` -- mirroring the
  existing statement for `artifacts/*` exactly, just a different prefix.

The one thing that had to be different from `/artifacts/*`: that prefix
has a bucket lifecycle rule that deletes everything after 2 days (it is
built for ephemeral *previews*). `uploads/*` has no such rule -- a
customer's uploaded photo has to persist, not expire like a preview link
does.

## Verified

- The SigV4 signing math was tested locally against the real bucket
  before this ever ran on the box, using the same signing function with
  local AWS CLI credentials standing in for IMDS's (identical logic,
  different credential source): a presigned `PUT` with a real image body
  succeeded, and -- the part that matters most -- a `PUT` with a
  `content-length` one byte off from what was signed for was rejected
  with `403`, proving the URL genuinely constrains the upload rather than
  merely gating the destination path.
- The full path was confirmed live end to end: sign -> `PUT` to S3 ->
  `GET` the same object back through `https://boxcode.sh/uploads/...`,
  actually served by CloudFront (`x-cache`/`via` headers present),
  `content-type` correct.

## Known limitations

- No per-project or per-user rate limiting or storage quota. A signed-in
  visitor could upload as many 5MB images as they like; nothing here
  bounds the total. Worth revisiting before this is exposed to real
  traffic at any volume.
- No image processing: no resizing, no re-encoding, no stripping EXIF/GPS
  metadata a photo might carry. Explicitly deferred, not forgotten --
  EXIF-with-GPS on a user-submitted photo is a real privacy question
  worth its own pass, not something to bolt on quietly here.
- Same "prove it works first" posture as the rest of `infra/`: the
  control-plane runs as root (though, unlike the others, it touches no
  local state for that root access to matter against).
