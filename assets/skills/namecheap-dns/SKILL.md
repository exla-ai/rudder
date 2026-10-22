---
name: rudder-namecheap-dns
description: Use for ANY DNS or domain change on a domain registered at Namecheap (nameservers dns1/dns2.registrar-servers.com) — pointing a domain or subdomain at Vercel, Netlify or a server, adding A/CNAME/TXT/MX or verification records, removing Namecheap's parking page, free email forwarding (hello@ to an inbox), and Namecheap API access setup. Makes the change itself through the Namecheap API and never hands the user DNS records to type into a dashboard.
---

# Namecheap DNS

**The rule: never give the user DNS records to enter by hand.** Making the change
is your job. If access is missing, the only acceptable ask is the one-time API
setup below, stated once, after everything else is done.

Tool: `<this skill dir>/scripts/namecheap-dns` (Python 3 stdlib, no installs).

```bash
NC="<this skill dir>/scripts/namecheap-dns"
```

## 1. Find where the domain's DNS lives

```bash
$NC lookup example.com NS        # DNS-over-HTTPS, works without dig
```

- `dns1/dns2.registrar-servers.com` → Namecheap BasicDNS → this tool.
- `*.vercel-dns.com` → `vercel dns add example.com <name> <type> <value>`.
- Netlify (`dns*.p0*.nsone.net`) → the Netlify CLI or API (`NETLIFY_AUTH_TOKEN`).
- Cloudflare, Route 53, anything else → that provider's CLI or API.

## 2. Change records: always list, dry-run, then apply

```bash
$NC list example.com                 # what's live, plus the email mode
$NC vercel example.com --dry-run     # read the plan
$NC vercel example.com               # backs up, writes, re-reads and verifies
```

Namecheap's `setHosts` **replaces every record on the domain**. Never call it
with curl or any other client: one call missing the MX or TXT records silently
breaks mail and verification. The tool merges with the live records, refuses to
round-trip types it doesn't understand (CAA, SRV), and keeps a backup of every
write in `~/.config/namecheap/backups/`. Undo any write with
`$NC restore <backup.json>`.

### Recipes

| Goal | Command |
|---|---|
| Apex + www to Vercel (removes the parking page) | `$NC vercel example.com`, then `vercel domains add example.com <project>` and `vercel domains add www.example.com <project>` |
| Subdomain to Vercel | `$NC vercel example.com --sub app` |
| Vercel ownership TXT | `$NC set example.com --add 'TXT _vercel "vc-domain-verify=..."'` |
| Subdomain to Netlify | `$NC set example.com --add 'CNAME app site-name.netlify.app.'` |
| Replace a conflicting record | add `--replace` (without it the tool refuses and shows the clash) |
| Remove a record | `$NC set example.com --remove 'CNAME old'` (value optional) |
| Free email forwarding | `$NC forward example.com --add hello=you@example.com` (refuses if MX mail is live, unless `--force`) |
| Account check | `$NC check` lists every domain and whether it's on Namecheap DNS |

### Verify

```bash
$NC list example.com                 # what Namecheap now serves (the write already re-read it)
$NC lookup example.com A             # the public view through Google/Cloudflare DNS, after the TTL
vercel domains inspect example.com   # the host's own check, when it has one
```

## 3. When access is missing

The tool reads `NAMECHEAP_API_USER` / `NAMECHEAP_API_KEY` (optionally
`NAMECHEAP_USERNAME`, `NAMECHEAP_CLIENT_IP`) from the environment or
`~/.config/namecheap/credentials.env`. On a Rudder cloud machine the environment
comes from the Rudder secrets vault; `env | grep -c NAMECHEAP` shows whether it
has them.

If the tool prints `NO_NAMECHEAP_CREDENTIALS`, or an API error naming an IP that
isn't whitelisted, first do everything else that doesn't need DNS (deploy,
attach the domain at the host, write docs). Then send the user one short
message with only this:

1. Namecheap → Profile → Tools → **Namecheap API Access** → ON. Namecheap
   requires a $50 balance, $50 spent in the last two years, or 20 domains.
2. **Whitelisted IPs** → add the IP the tool printed (this machine's public
   IPv4; only IPv4 works).
3. Store the key once, from their computer, so every Rudder cloud machine has
   it: `rudder cloud secrets set NAMECHEAP_API_USER <namecheap-username>` and
   `rudder cloud secrets set NAMECHEAP_API_KEY` (prompts for the value). For
   this machine only: `! <this skill dir>/scripts/namecheap-dns setup <namecheap-username>`
   (prompts for the key).

`setup` writes `~/.config/namecheap/credentials.env` (mode 600) and runs `check`.
Never print the key, commit it, put it in a URL or pass it as a command-line
argument. Never sign in to the Namecheap dashboard yourself: the account
password is the user's. A cloud machine's outbound IP can change on restart;
when Namecheap rejects a call, the error names the new IP. Whitelist it too, or
pin a static egress IP, which costs money and needs the user's go-ahead.

## Report

Say which domain, which records changed (before → after), where the backup is,
what `lookup` shows publicly, and anything still propagating.
