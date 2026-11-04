# Provider runtime and public discovery caches

Status: proposed startup contract, 2026-10-30; not enabled in alpha.10. No
provider defaults, user plugins, host configuration or credentials were changed.
This complements [managed launch](managed-launch.md),
[network isolation](managed-network-isolation.md) and
[remote composition](remote-managed-launch.md).

## Observed footprint and exact source

The [synthetic auth evidence](../../examples/agent-runtime/evidence/2026-10-30-managed-auth/README.md)
used the actual Codex CLI 0.166.0 executable, a fresh HOME/CODEX_HOME, synthetic
API-key auth and zero thread/model RPCs. The
[initial scan failure](../../examples/agent-runtime/evidence/2026-10-30-managed-auth/scan-bound-failure.json)
recorded 5,502 regular files / 87,111,201 bytes, including a verified
25,495,172-byte Git pack at codex/.tmp/plugins/.git/objects/pack/pack-*.pack.
The [successful streaming scan](../../examples/agent-runtime/evidence/2026-10-30-managed-auth/synthetic-codex-0.166.json)
covered 5,502 files / 87,112,164 bytes. No auth.json or synthetic-key bytes were
found. This is an auth-contract observation, not a controlled cold-start
benchmark, network-byte measurement or managed-helper/provider inference proof.

Upstream [rust-v0.166.0](https://github.com/openai/codex/releases/tag/rust-v0.166.0)
resolves to commit 6ff1e1ca3b30fc8b914720681fcf5fe62e95f856. The links below pin
that source; the evidence separately records the actual executable SHA256.

1. App-server defaults PluginStartupTasks to Start and
   [MessageProcessor construction](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/app-server/src/message_processor.rs#L557-L577)
   invokes maybe_start_plugin_startup_tasks_for_config before any thread request.
2. [The manager](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/core-plugins/src/manager.rs#L762-L782)
   starts curated repository sync when plugins are enabled, remote global catalog
   auth is unavailable and marketplace requirements allow the Git source. Fresh
   unauthenticated and API-key modes meet that auth condition;
   [ApiKey is not a Codex-backend mode](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/protocol/src/auth.rs#L53-L62).
   This does not require selecting a public plugin.
3. [Startup sync](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/core-plugins/src/startup_sync.rs#L23-L38)
   selects https://github.com/openai/plugins.git and CODEX_HOME/.tmp/plugins,
   its .tmp/plugins.sha and .tmp/plugins.sync.lock siblings. It resolves remote
   HEAD, initializes a staged repository, fetches depth 1 without tags and resets its checkout. The
   pack location matches the observed footprint; this does not attribute every
   observed byte solely to that repository.
4. [Account changes](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/app-server/src/request_processors/account_processor.rs#L279-L321)
   independently request curated sync after reloading current configuration.
   Suppressing only initial warmup would not cover the actual login sequence.

## Supported explicitly catalog-free profile

Use supported marketplace source requirements when the selected managed profile
explicitly needs no curated Git/API-key catalog. Keep features.plugins enabled
and preserve every explicitly approved plugin source required by that profile.
Do not silently apply this policy to existing personal/native conversations.

[Pinned MarketplacePolicy](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/core-plugins/src/marketplace_policy.rs#L54-L104)
enforces [marketplaces].restrict_to_allowed_sources and its allowed_sources map.
The manager checks that policy before starting curated sync, including the
post-login call. Excluding the curated Git URL also excludes runtime use/install
from that source; it is a capability choice, not a behavior-neutral warmup flag.
Bundled plugins and remotely installed workspace plugins are separate.

For an explicitly local/Git-catalog-free profile, the minimal requirements are:

```toml
[marketplaces]
restrict_to_allowed_sources = true
```

The empty allowlist rejects all local/Git marketplace sources. Other profiles
must add their selected approved local/Git rules. To omit only the curated
catalog, leave its Git URL out and ensure no broader host rule matches it.
Do not replace existing approved-source rules with an empty table. The official
[managed configuration guide](https://learn.chatgpt.com/docs/enterprise/managed-configuration#restrict-plugin-marketplace-sources)
documents this source restriction and the curated/bundled/workspace distinction.

The [pinned loader](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/config/src/loader/mod.rs#L108-L132)
loads Unix system requirements from /etc/codex/requirements.toml, before composed
cloud/legacy/admin requirements. CODEX_HOME/config.toml is ordinary user config;
CODEX_HOME/requirements.toml is not the system requirements path. A -c override
is not a substitute for this administrator policy. Check the effective composed
requirements: higher layers and additional source rules can change admission.

For managed execution, project a trusted immutable requirements file at the
guest's /etc/codex/requirements.toml before starting either app-server or its
frontend. This needs an explicit selected-policy projection in the execution
spec/root adapter; the current public-runtime allowlist does not provide that
directory. Bind the policy digest to the reserved profile and show a typed
unsupported result when unavailable. Do not edit the machine's real /etc, copy
the host's whole /etc/codex or inject the policy into shared user configuration.

## Insufficient switches

- [features.plugin_discovery_prefetch=false](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/core-plugins/src/manager.rs#L2884-L3024)
  is a supported discovery-prefetch control, but curated sync is started before
  that branch and independently of it. It does not prevent the observed pack.
- The [disable-plugin-startup-tasks-for-tests hook](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/app-server/src/main.rs#L67-L71)
  is compiled only with debug_assertions. It is not a production CLI solution;
  account-change sync is separate even for an embedding that skips startup tasks.
- features.plugins=false disables the subsystem broadly. Setting individual
  plugin enabled=false is also not a sync opt-out: cache refresh collects
  [configured plugin keys, not only enabled ones](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/core-plugins/src/loader.rs#L813-L852). Neither is a silent substitute
  for preserving the selected plugin contract. remote_plugin=false does not
  suppress the local curated fallback.

## Fallback when the public catalog is required

Build a content-addressed, immutable public template from a fresh verified
openai/plugins commit, never from an existing account's CODEX_HOME. Its manifest
binds provider/runtime version, platform, full public commit and file hashes.
Share only public package/runtime assets and this explicit allowlist:

- CODEX_HOME/.tmp/plugins checkout, including its clean public-only .git data.
- CODEX_HOME/.tmp/plugins.sha for the exact same commit.

Expose these as immutable lower data with a private CoW upper per attempt. Git
metadata, locks, refs/index, plugin state and every update remain privately
writable; no mutable Git directory, hardlinked writable checkout or whole HOME
is shared. Keep the containing .tmp directory privately writable: sync creates
lock/clone/backup siblings and atomically renames the checkout. A read-only bind
directly on the checkout is not a supported writable-cache replacement.

[The Git fast path](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/core-plugins/src/startup_sync.rs#L185-L229)
still checks remote HEAD but skips fetching/reset when the local commit matches
and .git is a directory. Do not strip .git: with installed trusted Git, a matching
SHA file alone does not satisfy that fast path. HEAD drift creates a new private
checkout; refresh shared templates off the launch path and retain old versions
for their owners. This cache reduces repeat materialization, not all network
lookups or update cost. Qualification must measure actual upper writes.

Never seed auth.json, OAuth/account snapshots, cloud policy tied to an account,
sessions/history/transcripts, logs, state databases, personal plugins, plugin
data, remote installed-plugin caches, lock files or abandoned staging. Do not
copy CODEX_HOME/plugins/cache wholesale merely because some entries are public.
Selected installed public plugins need their own explicit provenance/retention
manifest; selected private plugins remain private. Apply this pattern to another
provider only after its cache formats and update lifecycle are independently
qualified. Public immutable assets do not grant workspace or account authority.

## Qualification before changing launch behavior

| Case | Required result |
| --- | --- |
| Fresh synthetic API auth, no-curated profile, zero thread/model RPCs | Effective policy enforced before startup and after login; no curated fetch/pack; report all regular-file bytes/counts |
| Approved selected local/Git plugin | Its intended capabilities remain available; unrelated curated source rejected |
| Existing native/personal profile | Plugins/auth/config unchanged; no implicit migration to catalog-free policy |
| Malformed policy, missing projection or unexpected merged allow rule | Explicit refusal or reconciliation; no claimed opt-out |
| Same public commit, two private CoW attempts | Immutable lower hashes unchanged; no repeated pack materialization; isolated Git/state writes |
| Public HEAD changes, updater crash or concurrent attempts | Owned atomic update/recovery, bounded work and preserved prior template |
| Selected credentials, plugin installs and provider account changes | No account/session/private-plugin bytes in shared layer; no cross-attempt writes |

Rerun the bounded synthetic auth check and then the real managed helper/provider
gate after implementation. A fresh HOME alone does not establish a controlled
cold-start benchmark, and no source inspection here certifies startup latency.
