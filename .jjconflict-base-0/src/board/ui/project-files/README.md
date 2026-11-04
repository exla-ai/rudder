# Project files UI integration

Import `ProjectFileViewer`, `OpenProjectFile` and `createProjectFileTransport`
from `./project-files/index`. These modules require no new dependencies.

Construct one stable transport for the existing private route. For a local
board, inject `boardAccess().fetch.bind(boardAccess())`; for Cloud, inject the
existing authenticated private request function. The adapter sends GET queries
with `version=1`, `operation=list|read`, all four identity fields, and `path`.
It never adds bearer tokens to URLs or uses direct machine/browser reachability.

```tsx
const transport = createProjectFileTransport("/api/project-files", authorizedFetch);
const [selected, setSelected] = useState<ProjectFileReference | null>(null);

<OpenProjectFile reference={{ scope, path: "", kind: "folder" }} onOpen={setSelected} />
<OpenProjectFile reference={{ scope, path: output.path, kind: output.kind }} onOpen={setSelected} />
{selected && <ProjectFileViewer reference={selected} transport={transport} onClose={() => setSelected(null)} />}
```

`scope` is `{projectId, workspaceId, attemptId, ownerEpoch}` from current
authorized project/task state. Resolve output paths on the server; do not infer
workspace identity from the display name or accept absolute task output paths
as browser routes. `kind` may be `file` or `folder`; an empty path opens the root.
Keep task references attached to their producing attempt. When authorization
expires, unmount the viewer together with the existing board content.

The viewer reads one folder and optionally one selected file every two seconds,
with one cycle at a time, ten second cancellation and no filesystem watchers.
Errors clear old content and stop polling until Refresh. Navigation and identity
changes abort the previous selection. HTML asset rendering has its own sequential
refresh loop so saved CSS/image changes appear when HTML bytes do not change.

HTML and Markdown render in an empty sandbox with a restrictive CSP. Scripts,
navigation and nested content are disabled. Up to sixteen relative raster images
or stylesheets, totaling at most 12 MiB encoded, are fetched using the explicit
scope and embedded. Remote assets, SVG execution and CSS URL dependencies are
not enabled. Text/source is escaped. Supported images use data URLs. PDF previews
require the PDF MIME, strict base64, a 16 MiB limit and the `%PDF-` signature.
They use `data:application/pdf` documents with opaque origins and no referrer,
instead of Blob documents that inherit the control origin. The PDF iframe has
no sandbox because it can prevent native PDF viewers from working, as documented
by [MDN](https://developer.mozilla.org/en-US/docs/Learn_web_development/Core/Structuring_content/General_embedding_technologies)
and [WebKit issue 118859](https://bugs.webkit.org/show_bug.cgi?id=118859).
HTML/Markdown keep their empty sandbox and restrictive CSP.

Focused tests cover private transport identity, malformed/large responses,
source escaping, frame sandbox markup, image MIME policy, revision refresh,
timeout and cancellation. Real browser DOM asset rendering and PDF compatibility
still require the coordinator's browser/platform qualification.
