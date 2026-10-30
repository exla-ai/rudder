# Local browser board capability qualification

Passed 2026-10-30 23:14:49–23:14:51 UTC with Playwright 1.64.1 and
Chrome Headless Shell 156.0.8078.4. This is a disposable loopback qualification,
not a Cloud, managed guest network, real account, provider or release-image proof.

The server and minified UI/CSS were built privately with esbuild, with the
server beside its own assets. Root dist clean/build could not affect the run.
inputs.json records staged products and exact source input hashes;
source-manifest.sha256 is the separately frozen 19-file board lane envelope.
run.mjs is the exact successful private harness, with no saved credentials.

The runner used a cleared environment and explicit disposable HOME,
RUDDER_HOME, CLAUDE_CONFIG_DIR and CODEX_HOME. Its only provider session log
was a synthetic one-call fixture; no real account or global session log was
accessed. Node outbound fetch and browser context requests were restricted to
loopback. No Playwright trace, HAR, browser profile or storage database is
included. Screenshots show only fixture project/log/usage data.

result.json records anonymous denial for seven private endpoints; one-use
fragment exchange erased before first fetch; zero cookies; origin/instance
sessionStorage; escaped project/log rendering; authenticated live SSE; reload
without another ticket exchange; authenticated synthetic usage; stale-session
clearing after a same-port daemon instance restart; and recovery through a new
private bootstrap in a fresh document/tab. All 18 observed browser API calls
followed bearer rules, without credential queries, CSP violations or page errors.

Recovery follows the supported opener/new-document path. history preserves
sanitized pre-bootstrap and fragment-only navigation fixture failures; changing
only the URL fragment in an existing unavailable page does not rerun its entry
module. No product edit was made for that navigation case.

Tooling and browser runtime were private. Debian runtime packages were
downloaded and extracted privately, without a system package installation.
An inherited apt cache-clean hook attempted a global cache removal and was
denied; apt was not used again. The final run used private libraries and fonts.
All private fixture/runtime/tooling directories were deleted after this proof;
only this explicit evidence allowlist remains. Root owns the separate native
main/TUI integration check and any publication of this evidence.
