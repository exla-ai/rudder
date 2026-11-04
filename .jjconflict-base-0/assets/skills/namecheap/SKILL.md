---
name: rudder-namecheap
description: Use WHENEVER Namecheap comes up, and for any domain registered there (nameservers dns1/dns2.registrar-servers.com). Covers the whole account through the Namecheap API and CLI — DNS records (A/AAAA/CNAME/TXT/MX, verification records, pointing a domain or subdomain at Vercel, Netlify or a server, removing the parking page), free email forwarding (hello@ to an inbox), URL redirects, nameservers, registrar lock, domain privacy (WhoisGuard), domain list, expiry and auto-renew, contacts, availability and pricing, registering and renewing domains, transfers, account balance, and API access setup. Does the change itself and never hands the user records to type into the Namecheap dashboard.
---

# Namecheap

**The rule: do it through the API, never hand the user steps for the
dashboard.** If access is missing, the only acceptable ask is the one-time API
setup in section 6, stated once, after everything else is done.

Tool: `<this skill dir>/scripts/namecheap` (Python 3 stdlib, no installs).
`$NC --help` and `$NC <command> --help` list every option.

```bash
NC="<this skill dir>/scripts/namecheap"
$NC check            # credentials work + every domain in the account
```

## 1. Look before changing

| Question | Command |
|---|---|
| Which domains, when do they expire, auto-renew, privacy, lock | `$NC domains` (`--expiring`, `--expired`, `--search TEXT`) |
| One domain in full: status, expiry, privacy, nameservers, lock | `$NC info example.com` |
| Where its DNS is served from (public view) | `$NC lookup example.com NS` |
| Records Namecheap serves + email mode | `$NC list example.com` |
| Email forwards | `$NC forward example.com` |
| WHOIS contacts | `$NC contacts example.com` |
| Balance | `$NC balance` |
| Incoming transfers | `$NC transfers` |

If the nameservers are not `dns1/dns2.registrar-servers.com`, records live at
that DNS host, not at Namecheap: `*.vercel-dns.com` → `vercel dns add`;
Netlify (`dns*.p0*.nsone.net`) → the Netlify CLI/API; Cloudflare, Route 53 →
their CLI/API. Registrar settings (nameservers, lock, privacy, renewals) still
belong here.

## 2. DNS records: list, dry-run, then apply

```bash
$NC list example.com
$NC set example.com --add 'CNAME app site.netlify.app.' --dry-run
$NC set example.com --add 'CNAME app site.netlify.app.'      # backs up, writes, re-reads, verifies
```

Namecheap's `setHosts` **replaces every record on the domain**. Never call it
with curl or any other client: one call missing the MX or TXT records silently
breaks mail and verification. The tool merges with the live set, refuses record
types it cannot round-trip (CAA, SRV: tell the user those need the dashboard),
and backs up every write to `~/.config/namecheap/backups/`. Undo with
`$NC restore <backup.json>`.

| Goal | Command |
|---|---|
| Apex + www to Vercel (removes the parking page) | `$NC vercel example.com`, then `vercel domains add example.com <project>` and `vercel domains add www.example.com <project>` |
| Subdomain to Vercel | `$NC vercel example.com --sub app` |
| Verification TXT (Vercel, Google, …) | `$NC set example.com --add 'TXT _vercel "vc-domain-verify=..."'` |
| Subdomain to Netlify / any host | `$NC set example.com --add 'CNAME app site-name.netlify.app.'` |
| A record to a server | `$NC set example.com --add 'A api 203.0.113.7'` |
| Redirect a name (301) | `$NC set example.com --add 'URL301 old https://new.example.com'` |
| Third-party mail (MX) | `$NC set example.com --add 'MX @ mx1.provider.com. 1800 10'` |
| Replace a conflicting record | add `--replace` (without it the tool refuses and shows the clash) |
| Remove a record | `$NC set example.com --remove 'CNAME old'` (value optional) |
| Free email forwarding | `$NC forward example.com --add hello=you@example.com` (refuses to cut off live MX mail unless `--force`) |

Verify with `$NC list` (Namecheap's view, already re-read after the write) and
`$NC lookup example.com A` (public DNS, after the TTL).

## 3. Registrar settings

| Goal | Command |
|---|---|
| Move DNS to another host (Vercel, Cloudflare, …) | `$NC nameservers example.com --set ns1.host.com ns2.host.com --yes` |
| Back to Namecheap BasicDNS | `$NC nameservers example.com --default --yes` |
| Transfer lock | `$NC lock example.com --on` / `--off` (off only to transfer it out) |
| Domain privacy | `$NC privacy example.com --on` / `--off`; `$NC privacy` lists all |

Switching nameservers moves the whole domain: copy the records to the new host
first, or mail and verification break. The tool asks for `--yes` for that reason.

## 4. Buying and renewing (spends money)

```bash
$NC available idea.dev other.com      # free? premium?
$NC price dev                         # your register price; --action renew|transfer, --years N
$NC renew example.com --years 1       # prints price + balance, charges nothing
$NC register idea.dev --contacts-from example.com   # same: prints, charges nothing
```

Anything that charges the account does nothing without `--yes`. **Before
adding `--yes`, tell the user the item and total and get a clear yes in chat**,
every time; a yes for one purchase does not cover the next. Registration copies
WHOIS contacts from a domain the user already owns and turns free privacy on.
Premium names are refused (they need the dashboard). Payment comes from the
account balance; if it is short, say so and stop.

## 5. Report

Say what changed (before → after), the backup path for record writes, what
`lookup` shows publicly, anything still propagating, and for purchases the
amount charged and the order id.

## 6. When access is missing

The tool reads `NAMECHEAP_API_USER` / `NAMECHEAP_API_KEY` (optionally
`NAMECHEAP_USERNAME`, `NAMECHEAP_CLIENT_IP`) from the environment or
`~/.config/namecheap/credentials.env`. On a Rudder cloud machine the environment
comes from the Rudder secrets vault; `env | grep -c NAMECHEAP` shows whether it
has them.

If the tool prints `NO_NAMECHEAP_CREDENTIALS`, or an API error naming an IP that
isn't whitelisted, first do everything that doesn't need Namecheap. Then send
the user one short message with only this:

1. Namecheap → Profile → Tools → **Namecheap API Access** → ON. Namecheap
   requires a $50 balance, $50 spent in the last two years, or 20 domains.
2. **Whitelisted IPs** → add the IP the tool printed (this machine's public
   IPv4; only IPv4 works).
3. Store the key once, from their computer, so every Rudder cloud machine has
   it: `rudder cloud secrets set NAMECHEAP_API_USER <namecheap-username>` and
   `rudder cloud secrets set NAMECHEAP_API_KEY` (prompts for the value). For
   this machine only: `! <this skill dir>/scripts/namecheap setup <namecheap-username>`
   (prompts for the key).

Never print the key, commit it, put it in a URL or pass it as a command-line
argument. Never sign in to the Namecheap dashboard yourself: the account
password is the user's. A cloud machine's outbound IP can change on restart;
when Namecheap rejects a call, the error names the new IP. Whitelist it too, or
pin a static egress IP, which costs money and needs the user's go-ahead.
