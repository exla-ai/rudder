// Release evidence only. These names are not engine operations or authority.
export const checks = {
  'sdk-local': { command: ['scripts/test-sdk-helper-local.sh', '--real'], scope: 'local-helper', platform: 'linux', limits: 'Real pinned Python/Node/TypeScript; UID1500 parent substitutes for Root. No production SDK admission.' },
  'app-lifecycle': { command: ['/workspace/rudder-build/rcargo', 'test', '-p', 'rudder-native', '--test', 'app-engine-lifecycle', 'app_engine_lifecycle::tests::'], scope: 'local-process', limits: 'Actual App/socket/PTY with fake model backends. No authenticated model or Mac proof on Linux.' },
  'restart-drain': { command: ['/workspace/rudder-build/rcargo', 'test', '-p', 'rudder-engine', '--lib', 'restart_recovery'], scope: 'local-process', limits: 'Real local child exits and crash recovery; no replacement worker or managed Root admission.' },
  'multi-process-cache': { command: ['/workspace/rudder-build/rcargo', 'test', '-p', 'rudder-engine', '--test', 'multi_agent_cache_execution'], scope: 'local-process', limits: 'Independent local worker processes and host C; not real coding models or independent volumes.' },
  'production-ts': { command: ['/workspace/rudder-build/rnpm', 'exec', '--', 'node', '--test', 'tests/v4-production-integration.test.mjs'], scope: 'local-process', limits: 'Local production adapters and protocol fixtures, no browser or provider.' },
  'migration-ts': { command: ['/workspace/rudder-build/rnpm', 'exec', '--', 'node', '--test', 'tests/v4-migration-production.test.mjs'], scope: 'local-process', limits: 'Disposable local migration; existing live users and offline Macs need separate drills.' },
  'readiness-tests': { command: ['/workspace/rudder-build/rnpm', 'exec', '--', 'node', '--test', 'tests/v4-release-readiness.test.mjs'], scope: 'tooling', limits: 'Evidence verifier regression only. Does not qualify any product experiment.' },
};

// Each requirement is an observation the coordinator must review. Local test
// receipts are useful supporting evidence, but cannot satisfy these by name.
export const gates = [
  ['E0', 'Representative baselines', ['baseline-linux', 'baseline-dependency-js', 'baseline-python', 'baseline-mac-arm64', 'baseline-mac-x64'], 'Local execution suffices for Linux/JS/Python; native Mac hardware is required for Xcode.'],
  ['E1', 'Fork isolation and scaling', ['fork-linux', 'fork-mac-arm64', 'fork-mac-x64'], 'Local dedicated Linux and real APFS Macs suffice; separate fallback volumes and 1/10/100 views.'],
  ['E2', 'Distribution and publication', ['two-worker-distribution'], 'Requires two independently provisioned Fly workers and empty destination storage, actual builds, faults and retention races. This home alone cannot pass.'],
  ['E3', 'Enforced build inputs and cache soundness', ['cache-soundness', 'managed-sdk-runtime'], 'Local deterministic soundness matrix is useful; genuine SDK/Root and runtime admission are still unqualified. A qualified local Root runner can prove the mechanism; the production image path requires the retained owner.'],
  ['E4', 'Warmed execution decision', ['warm-service-decision'], 'Local matched timings and an explicit defer/adopt ADR suffice. A memory snapshot decision needs its own qualified host.'],
  ['E5', 'Filesystem and durability', ['filesystem-linux', 'filesystem-mac-arm64', 'filesystem-mac-x64', 'replacement-root-restore', 'managed-sdk-runtime'], 'Local fault fixtures cannot prove loss of the original volume; independent replacement restore is required. Mac semantics need real Macs.'],
  ['E6', 'Actual communication and peer controls', ['communication-codex', 'communication-claude', 'communication-support-policy'], 'Real supported installed models can run locally, including busy/idle/policy-disabled and control races; no provider provisioning is inherently needed.'],
  ['E7', 'Git and jj compatibility', ['vcs-linux', 'vcs-mac-arm64', 'vcs-mac-x64'], 'Disposable local repos suffice per platform. Pin jj 0.40.0; retain actual Git version, worktree/submodule/conflict/export/undo samples.'],
  ['E8', 'Authority, offline ownership and rollback', ['authority-faults', 'offline-mac-handoff', 'migration-rollback'], 'Local partition/crash simulations support epochs; a real disconnected Mac and separate owners are needed for the handoff drill. No Fly provisioning is inherently required.'],
  ['E9', 'Capacity and cost', ['capacity-envelope'], 'Dedicated local resources can establish an honest bounded envelope. 1,000 views are not 1,000 agents; larger claims require actual resources and quotas.'],
  ['E10', 'Product and release readiness', ['surface-projections', 'migration-rollback', 'replacement-root-restore', 'managed-sdk-runtime', 'release-ci', 'mac-install-arm64', 'mac-install-x64', 'mac-app-signing'], 'Full supported CI, real Mac installation/launch/signing, all surfaces, daemon-free recovery and replacement-worker restore remain required.'],
  ['E11', 'Commands and obsolete machinery', ['command-journeys', 'legacy-retirement'], 'Local CLI/TUI plus actual web/phone/Mac workflows can qualify; fixture or registry counts alone cannot.'],
  ['E12', 'Routing and task ownership', ['routing-corpus', 'routing-surfaces'], 'Local labelled corpus and real supported surface/backend ingress suffice, including fallback and explicit approval/budget scope.'],
  ['E13', 'Laptop previews, terminals and files', ['laptop-private-preview', 'laptop-terminal', 'file-viewer-browser'], 'Requires a real laptop, authenticated private path and actual terminal/browser observations. Existing home may be used without provisioning.'],
  ['E14', 'Model efficiency and quality', ['efficiency-codex', 'efficiency-claude'], 'Requires matched real-model task samples and accepted/rejected outcomes. Local authenticated sessions suffice; synthetic comparator tests cannot pass.'],
].map(([id, title, requirements, locality]) => ({ id, title, requirements, locality }));

// These are scopes, not assertions that a particular experiment passed.
export const requiredScopes = {
  'two-worker-distribution': 'two-fly-workers',
  'replacement-root-restore': 'replacement-worker',
  'managed-sdk-runtime': 'managed-root',
  'communication-codex': 'real-codex',
  'communication-claude': 'real-claude',
  'efficiency-codex': 'real-codex',
  'efficiency-claude': 'real-claude',
  'laptop-private-preview': 'real-laptop',
  'laptop-terminal': 'real-laptop',
  'file-viewer-browser': 'real-browser',
};

export const deliveryRequirements = ['release-ci', 'registry-install', 'cloud-deployment',
  'mac-install-arm64', 'mac-install-x64', 'mac-app-signing'];
