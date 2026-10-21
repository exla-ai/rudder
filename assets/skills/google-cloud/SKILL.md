---
name: rudder-google-cloud
description: Use for any Google Cloud or Google API setup — creating or naming a GCP project, enabling APIs (Gmail, Calendar, Drive, Gemini, …), "Sign in with Google" / Google OAuth clients, the consent screen (app name, logo, domains, scopes, verification, test users), redirect_uri_mismatch and other OAuth errors, gcloud auth, service accounts and keys, and storing the resulting secrets. Makes sure users see the product's own name on Google's screens, never a stray project like "My First Project".
---

# Google Cloud, done properly

The failure this skill exists to prevent: a user clicks "Sign in with Google" and
sees **"to continue to e2mail-494301"** or **"My First Project"** (or an
unverified-app wall) because the OAuth client was dropped into whatever project
was lying around. Every step below ends with a check of what a real user sees.

Helper: `node <this skill dir>/scripts/gcp.mjs <command>` (Node 18+, no deps).
It finds credentials from `gcloud`, gcloud's stored login, or ADC; run
`whoami` first.

## The rules

1. **One project per product, named after the product.** Display name = the
   product name exactly as marketed ("Tempo"); project ID = `<product>-<org>`
   (immutable, globally unique, 6–30 chars). Never reuse another product's
   project, a "My First Project", or a `gen-lang-client-*` Gemini project.
   Create it under the user's organization (the helper picks the only org
   automatically).
2. **The consent screen's App name is what users read**, not the project name
   and not the client name. Set it on the Branding page to the product name,
   with the product's own support email, home page, privacy and terms URLs, and
   the product's domain as an authorized domain.
3. **Ask for the narrowest scopes that work.** Basic (openid/email/profile) can go
   straight to production. Sensitive (Calendar, gmail.send) need verification.
   Restricted (Gmail read/modify/compose, Drive full) need verification **and**
   a CASA security assessment before more than 100 users; until then keep the
   app in Testing and add test users. Tell the user this up front.
4. **Secrets never touch chat or git.** Read the client secret once, write it
   straight into the host's secret store (`fly secrets set`, `vercel env add`,
   GitHub secrets), and refer to it by name afterwards.
5. **Done means verified from outside**: `check-client` passes for every
   redirect URI with the expected app name, and one real sign-in in a browser
   reaches the app.

## Workflow

```bash
G="node <skill dir>/scripts/gcp.mjs"
$G whoami                                     # account, scopes, org
$G projects --filter <product>                # is there already a right project?
$G ensure-project --id <product>-<org> --name "<Product>"   # create, or fix the display name
$G enable --project <id> --apis gmail,calendar              # aliases: drive sheets docs people gemini vertex run storage …
$G audit --project <id>                       # name, parent, enabled APIs
$G oauth-plan --project <id> --name "<Product>" --home https://<domain> \
   --support <owner email> --redirect https://<domain>/<callback> [--redirect …] \
   [--scopes gmail.readonly,calendar.readonly]
```

`oauth-plan` prints the exact Console pages and the value for every field.
**Branding, Audience, Data access and Clients have no public API** (the old
`gcloud alpha iap oauth-*` commands were shut down in March 2026; `gcloud iam
oauth-clients` is for workforce identity, not "Sign in with Google"). Do them:

- **With browser tools** (Claude in Chrome or the built-in browser): open each
  URL from the plan, confirm the signed-in account chip is an owner of the
  project, fill the fields verbatim, save. On the client page, copy the ID and
  secret before closing the dialog — the secret is not shown again. Never type
  the user's Google password; if the browser is signed out, stop and ask the
  user to sign in.
- **Without a browser**: hand the user the plan output as a checklist and ask
  them to paste back the client ID and secret, or to set the secrets on the
  host themselves (`! fly secrets set …`).

Then store and verify:

```bash
fly secrets set -a <app> GOOGLE_CLIENT_ID=<id> GOOGLE_CLIENT_SECRET=<secret>   # or vercel env add / gh secret set
$G check-client --client-id <id> --expect-name "<Product>" \
   --redirect https://<domain>/<callback> [--redirect …]
```

`check-client` asks Google's real authorization endpoint, signed out, exactly
as a new user's browser would. It catches `redirect_uri_mismatch`,
`invalid_client`, `deleted_client`, Internal-only audiences and a wrong app
name ("continue to e2mail-494301") without anyone signing in. Finish with one
real sign-in in a browser and say in your report what the screen showed.

## Getting the details right

- **Redirect URIs match byte for byte**: scheme, host, port, path and trailing
  slash. `https` only, except `http://localhost`/`127.0.0.1` for local dev. Add
  the production URI and the local one if the app is developed locally; keep
  staging on its own client if its domain differs.
- **Authorized JavaScript origins** are only for the Google Identity Services
  button/One Tap (browser-side flows); server-side redirect flows don't need them.
- **Native apps** (iOS/Android/desktop) get their own client of that type; iOS
  uses the reversed client ID as a URL scheme. Don't reuse the web client.
- **Testing vs production**: in Testing only listed test users can sign in and
  refresh tokens expire after 7 days — fine for a demo, wrong for real users.
  Publish as soon as the scopes allow it.
- **Logo**: uploading one triggers brand verification; leave it off until the
  user is ready to verify. The name, links and domain still show.
- **Internal user type** restricts sign-in to the org's Workspace accounts — only
  right for internal tools.
- **Verification** (sensitive/restricted scopes): needs the home page on the
  authorized domain, a privacy policy describing Google data use (Limited Use),
  a demo video of the consent flow, and for restricted scopes a CASA
  assessment. Domain ownership is verified in Search Console by the same
  account. Note it as a follow-up with the 100-user cap; don't silently ship.
- **Service accounts** for server-to-server: prefer workload identity / attached
  service accounts over JSON keys; if a key is unavoidable, create it with
  `gcloud iam service-accounts keys create` straight into the secret store and
  delete the local file.
- **Billing** isn't needed for OAuth, Gmail or Calendar; it is for Maps, Vertex,
  Cloud Run and most paid APIs. Check before promising those.
- **Renaming later** is safe for the project display name (`ensure-project`
  again) and the consent App name (Branding page). The project ID can never
  change — choose it once, deliberately.

## Report

Say which project (ID and display name), which APIs, which client (ID, never
the secret), where the secrets live, the `check-client` result per redirect
URI, what the real sign-in screen showed, the publishing status, and any
verification still owed (with the user cap it implies).
