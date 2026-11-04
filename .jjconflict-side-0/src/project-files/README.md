# Project files registration handoff (n13)

The implementation adds no listeners or dependencies. All files are new modules
in the assigned paths. Shared route/UI/native registration belongs to n13.

## Local board

In `src/board/daemon.ts`, inside the authenticated `handleProjectApi` after
`findProjectBySlug` resolves the route's registered project, mount GET
`/api/projects/:slug/files` with `handleProjectFiles(req, res, url, service)` from
`src/project-files/http.ts`. Keep the existing loopback, origin, bearer and session
checks. This is a read operation; a projector may serve it without scheduling.

Create/reuse `ProjectFileService` from `service.ts`. Its constructor accepts
`authorize(scope, signal) => Promise<{scope, root}>`. Resolve every grant from
current engine project/workspace/attempt records, plus the route's registered
project and authenticated session. Validate all four requested fields, project
membership, the current owner epoch, and whether the retained attempt's workspace
is still available. The root is the trusted workspace assignment for that exact
attempt, never a query field, display name, inferred directory, or fallback to the
board owner's root. Do not fabricate an attempt or epoch for legacy records.
If the authoritative binding is unavailable, return access unavailable.

The callback runs both before reading and before publication. It must recheck
`state.authority.valid(session)` each time and throw `ProjectFileError("denied",
"Project file access expired")` on revoked access. Cache the filesystem service
per bounded authenticated context, rather than constructing an unbounded pool
per request. Dispose contexts when board sessions expire. Every service permits
eight simultaneous operations; also retain a host-wide request limit.

The GET query is versioned: `version=1&operation=list|read&projectId=...&workspaceId=...
&attemptId=...&ownerEpoch=...&path=...`. All fields, including the empty root path,
are required exactly once. Responses are `FileListing` or `FileDocument` from
`types.ts`. JSON only, no raw HTML/PDF routes. Text is at most 4 MiB; checked raster
images and PDFs are at most 16 MiB, encoded as base64. Folder enumeration stops at
500 entries and reports truncation. Filesystem calls are asynchronous; no App
or browser render loop performs filesystem work.

## Private Cloud

Register GET `/api/app/project-files` inside `handleHomeApi` after existing
account authentication. `cloud/src/project-files/relay.ts` exports
`CloudProjectFiles<Actor>`; one process-level instance caps concurrent queries.
Call `query(authenticatedActor, url, abortSignal)` and respond with JSON/no-store/
nosniff, mapping `CloudProjectFileError.status`. Do not expose raw exceptions,
private roots, tokens or provider errors. GET only.

Its `authorize(actor, scope, signal)` callback must call the existing
`requireHomeRepo(accountId, scope.workspaceId)` account/project membership check
and the current authority's project/workspace/attempt/owner binding lookup. Return
`{scope, homeId}` only if these identities agree. An authenticated tailnet, a
reachable IP, or a matching port is insufficient. The callback runs again after
RPC; ownership or home movement rejects the response.

Adapt `rpc(homeId, "projectFiles", request, signal)` to existing authenticated
`homeRpc`. On the home, register `projectFiles` to dispatch the v1 request through
the same `ProjectFileService`, with a grant resolver using the home's current
authoritative assignment records. Dispatch against the selected workspace, not
the supervisor's cwd. This requires a small `cloud/worker/supervisor.mjs` RPC case
owned by the registration/integration node. No direct filesystem access occurs on
the control plane. Preserve existing relay fallback; no n12-specific preview or
terminal API is required.

Bound private RPC response frames to 24 MiB for this operation and propagate
abort/deadline to pending home requests. Existing generic RPC limits may need an
operation-specific allowance for base64 PDFs/images; keep other limits intact.
Both relay and service have ten second deadlines and cancelable authorization.

## Project tree, task output and native action

Register the components described in
`../board/ui/project-files/README.md`. A project root button uses the selected
authorized workspace/attempt scope and `{path: "", kind: "folder"}`. Each
structured task output includes its producing workspace/attempt, the currently
authorized owner epoch, and a relative path/kind. `taskFileReference` from
`references.ts` binds a structured output to a trusted root and rejects paths
outside it. Do not automatically turn arbitrary terminal text into file links.

`projectFileLink("/rudder/:slug", reference)` produces a credential-free deep
link. On board load, read `file`, optional `fileKind`, and the four identity
fields into the selected reference; all reads still go through the private API.
Clear selected content on session loss, project switch or ownership rejection.

`openProjectFile(repoRoot, reference)` from `open.ts` uses the existing private
board host socket to get a one-use browser ticket and opens the target directly.
It never prints a ticket. Native task-output/folder actions can run
`node <installed-dist>/project-files/open.js <trusted-project-root> <reference-json>`
on an existing bounded background helper thread. Send success/failure as a typed
result to the App loop; do not launch/wait from render or input handling. Reuse
the normal browser opening path for Cloud on the laptop. The helper does not
deploy a project or access its filesystem without browser authorization.

## Verification and scope

`tests/project-files.test.mjs` exercises actual temporary project files, local
HTTP JSON handling, the Cloud relay to the same filesystem service, explicit
identity, cross-project/stale rejection, symlink escapes, delete/overwrite/rename
races, large payload bounds, cancellation and saved revisions.
`tests/project-files-viewer.test.mjs` covers source escaping, sandbox markup,
scoped asset paths, private transport validation, polling cancellation and update
observation. Its PDF fixture validates dispatch/type handling, not a complete PDF
render. These focused tests compile only these modules with existing esbuild and
do not write shared dist output.

Remaining integration/qualification: the route and component registrations above,
real authoritative engine binding adapters, native task action registration, and
real browser HTML asset/PDF rendering on local and authorized Cloud paths. No
Fly, hardware, platform, release or deployment gate is claimed by this worker.
HTML scripts, remote assets, SVG execution, CSS URL dependencies and file editing
are outside this change. Saved changes made by agents/editors are observed.
