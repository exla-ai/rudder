import { render } from "preact";
import { BoardView, ProjectIndex } from "./components";
import { UsageView } from "./usage";
import { boardAccess } from "./access";

declare global {
  interface Window {
    __RUDDER_CONTROL_MODE__?: "projector" | "scheduler";
    __RUDDER_CAN_MUTATE__?: boolean;
  }
}

function App({ slug, usage }: { slug: string; usage: boolean }) {
  if (usage) return <UsageView />;
  return slug ? <BoardView slug={slug} /> : <ProjectIndex />;
}

const mount = document.getElementById("app");
if (mount) {
  const unavailable = () => render(<main class="index-main"><p role="alert">Open this board from Rudder to connect.</p></main>, mount);
  const access = boardAccess();
  access.onLost(unavailable);
  void access.initialize().then((info) => {
    const usage = window.location.pathname === "/usage" || window.location.pathname === "/rudder/usage";
    const match = /^\/rudder\/([^/]+)\/?$/.exec(window.location.pathname);
    let slug = "";
    try { if (match && !usage) slug = decodeURIComponent(match[1]); } catch { /* Invalid routes stay at the index. */ }
    window.__RUDDER_CONTROL_MODE__ = info.control_mode;
    window.__RUDDER_CAN_MUTATE__ = info.scope.owner_project_control && slug === info.owner_slug;
    document.title = usage ? "rudder · usage" : "rudder";
    render(<App slug={slug} usage={usage} />, mount);
  }).catch(unavailable);
}
