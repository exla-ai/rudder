---
name: rudder-google-cloud
description: Use for any Google Cloud or Google API setup — creating or naming a GCP project, enabling APIs (Gmail, Calendar, Drive, Gemini, …), "Sign in with Google" / Google OAuth clients, the consent screen (app name, logo, domains, scopes, brand verification, test users), moving sign-in to another domain, redirect_uri_mismatch and other OAuth errors, gcloud auth, service accounts and keys, and storing the resulting secrets. Makes sure users see the product's own name on Google's screens, never a stray project like "My First Project".
---

# Google Cloud, done properly

The failure this skill exists to prevent: a user clicks "Sign in with Google" and
sees **"to continue to e2mail-494301"** or **"My First Project"** (or an
unverified-app wall) because the OAuth client was dropped into whatever project
was lying around. Every step below ends with a check of what a real user sees.

Helper: `node <this skill dir>/scripts/gcp.mjs <command>` (Node 18+, no deps).
It finds credentials from `gcloud`, gcloud's stored login, or ADC; run
`whoami` first.

**Google's official skills cover the rest.** This skill is the product layer
(project naming, consent screen, OAuth client, what a user sees). For general
`gcloud` work use Google's `gcloud` skill (check leaf syntax with `gcloud help
<command>` before running it, `--limit`/`--filter`/`--format` on every list,
`--quiet`, dry-run where supported) and `google-cloud-recipe-auth` for IAM,
service accounts and ADC. Both are in the `google-cloud-developer` plugin; if
they are not loaded: `claude plugin marketplace add google/skills && claude
plugin install google-cloud-developer@google-plugins`.

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
   the product's domain as an authorized domain. **It only shows once the
   branding is verified.** Until then Google shows the redirect URI's domain:
   the registrable domain for your own (`app.rudder.exla.ai` → "exla.ai"), the
   whole host for a platform one ("rudder-cloud-control.fly.dev"). See "Brand
   verification" below.
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
- **Logo**: uploading one adds a logo review to verification; leave it off
  unless the user wants it, since basic scopes verify faster without one.
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

## Brand verification (getting the App name on screen)

Branding page → **Verify branding**. Google checks that every **authorized
domain** is verified in Search Console by an owner of the project, that the
home page and privacy policy live on those domains and name the app, and that
the support email belongs to the owner. Basic scopes: usually a few days.

- **Redirect URIs must be on a domain the user owns.** Platform hosts
  (`*.fly.dev`, `*.vercel.app`, `*.run.app`, `*.awsapprunner.com`,
  `*.netlify.app`) can never be verified, and while they are authorized
  domains verification fails. Serve the auth callback from the product's own
  domain (e.g. `app.<domain>`), then remove the platform redirect URIs and
  their authorized domains.
- **Search Console**: add a Domain property for the apex and verify it with
  the DNS TXT record it gives (through the DNS host's API if you have it,
  otherwise ask the user), signed in as the same account that owns the project.

## Moving sign-in to another domain

Order matters. Switching the app first breaks sign-in for everyone.

1. Point the new host at the app (DNS + TLS) and check it serves.
2. Add the new redirect URI (and JS origin if used) to the Google client **and**
   to every other provider on the same auth base URL (GitHub OAuth/App callback
   URLs, Slack, Linear …). `check-client` must show it accepted.
3. Make every page that STARTS a sign-in redirect to the new host before
   starting it: the OAuth state cookie is set on the host that starts the flow
   and must be there when the callback lands. Native shells and old CLIs keep
   using the old address, so bounce their login pages; don't strand them.
4. Flip the auth base URL (`BETTER_AUTH_URL`, `NEXTAUTH_URL`, …) and deploy.
5. Real sign-in on the new host for each provider; then remove the old redirect
   URIs and authorized domains, and verify the branding.

## Keeping an existing client

A client already shipped in apps (an iOS bundle, a published CLI, stored in a
host's secrets) can stay in a misnamed project when moving it would change the
client ID. Its consent screen can still say the product's name: set the
Branding and verify it. Note the exception in the report; never silently.

## In the browser

- Google may ask for a passkey, security key or password re-auth (the URL
  carries `rapt=`). Stop and ask the user to complete it; never type it.
- The client page's "Last used date" lags by days. Confirm which client is live
  from the app's own sign-in redirect (`curl -sI <login url>` → `client_id=`),
  not from that date.
- Settings "may take 5 minutes to a few hours"; `check-client` usually sees a
  new redirect URI within a minute.

## Report

Say which project (ID and display name), which APIs, which client (ID, never
the secret), where the secrets live, the `check-client` result per redirect
URI (including the name Google shows today), what the real sign-in screen
showed, the publishing and brand-verification status, and any verification
still owed (with the user cap it implies).
