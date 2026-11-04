# Startup context

Updated 2026-10-30. These measurements describe prompt size and behavior; they
do not establish a model-latency or task-completion speedup.

Rudder injects a short role contract, then the user's task. Main owners get
checkout ownership, verification and delivery responsibilities. Isolated workers
get their assignment boundary and jj integration rules. Both get concise
coordination guidance and the completion-report contract. Discussion input keeps
its read-only contract.

| Automatically supplied content | Before | Current |
|---|---:|---:|
| Main-owner contract | 2,641 bytes | 1,167 bytes |
| Isolated-worker contract | 3,238 bytes | 915 bytes |
| This repository's `AGENTS.md` | 193,536 bytes | 5,820 bytes |

Sizes are UTF-8 bytes, excluding task text. The role-contract numbers come from
the actual Rust `concat!` literals, including the shared parallel-work rule.
The complete engineering reference remains in
[engineering-reference.md](../engineering-reference.md); `AGENTS.md` points to
the relevant sections. Reading that reference is driven by the current change.

Execution contracts carry instruction-file references rather than another copy
of files the backend already loads. The tool-less API planner receives bounded
AGENTS/CLAUDE excerpts: at most 6,000 characters from one file and 8,000 in total,
with visible truncation. README and runtime coordination remain references.

An agent reads applicable rules once. It revisits coordination when ownership
overlaps, a relevant decision changes, or work is about to integrate. Rudder no
longer tells every agent to reread all shared files before each significant
step. Accepted decisions and task history remain available on demand.
Generated recent requests are explicitly reference history: they can belong to
other runs and do not replace the current assignment. Identical task/current
prompt text is emitted once on each roster row.

CLI startup also avoids unrelated setup for version/help, empty noninteractive
input and private callbacks. The native platform probe reads at most 4 KiB of
executable headers instead of loading the whole binary; the existing ELF and
Mach-O architecture checks remain in place. These are bounded startup work
changes, not measured model speedups.

Verification covers ownership and delivery rules, discussion scope, preserved
user tasks, planner excerpt limits, shared rule consistency, and compact prompt
budgets. The current context audit reports no oversized, duplicated or
credential-like context findings. Credential detection still recognizes actual
provider-key patterns; a task slug containing `task-` does not count as an API
key.
