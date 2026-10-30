import { streamBoardEvents } from "./events";
import { useEffect, useReducer, useRef, useState } from "preact/hooks";
import {
  type ActivityEntry,
  type BoardEdge,
  type BoardNode,
  type BoardSnapshot,
  type MemoryEntry,
  type PlanGate,
  eventsUrl,
  fetchState,
} from "./types";

// ---------------------------------------------------------------------------
// Reducer store. nodes are a Map keyed by id for O(1) upsert/remove; the rest
// are replaced wholesale on SNAPSHOT / MEMORY frames.
// ---------------------------------------------------------------------------

export type BoardState = {
  nodes: Map<string, BoardNode>;
  edges: BoardEdge[];
  gates: PlanGate[];
  memory: MemoryEntry[];
  activity: ActivityEntry[];
  loaded: boolean;
};

export type Action =
  | { type: "SNAPSHOT"; snapshot: BoardSnapshot }
  | { type: "NODE_UPSERT"; node: BoardNode }
  | { type: "NODE_REMOVE"; id: string }
  | { type: "EDGE_UPSERT"; edge: BoardEdge }
  | { type: "EDGE_REMOVE"; from: string; to: string }
  | { type: "MEMORY"; memory: MemoryEntry[] }
  | { type: "ACTIVITY"; activity: ActivityEntry[] };

export function initialState(): BoardState {
  return { nodes: new Map(), edges: [], gates: [], memory: [], activity: [], loaded: false };
}

export function reducer(state: BoardState, action: Action): BoardState {
  switch (action.type) {
    case "SNAPSHOT": {
      const nodes = new Map<string, BoardNode>();
      for (const n of action.snapshot.nodes) nodes.set(n.id, n);
      return {
        nodes,
        edges: action.snapshot.edges ?? [],
        gates: action.snapshot.gates ?? [],
        memory: action.snapshot.memory ?? [],
        activity: action.snapshot.activity ?? [],
        loaded: true,
      };
    }
    case "NODE_UPSERT": {
      const nodes = new Map(state.nodes);
      nodes.set(action.node.id, action.node);
      return { ...state, nodes };
    }
    case "NODE_REMOVE": {
      if (!state.nodes.has(action.id)) return state;
      const nodes = new Map(state.nodes);
      nodes.delete(action.id);
      return { ...state, nodes };
    }
    case "EDGE_UPSERT": {
      const edges = state.edges.filter(
        (edge) => !(edge.from === action.edge.from && edge.to === action.edge.to && edge.kind === action.edge.kind),
      );
      edges.push(action.edge);
      return { ...state, edges };
    }
    case "EDGE_REMOVE":
      return {
        ...state,
        edges: state.edges.filter((edge) => !(edge.from === action.from && edge.to === action.to)),
      };
    case "MEMORY":
      return { ...state, memory: action.memory };
    case "ACTIVITY":
      return { ...state, activity: action.activity };
    default:
      return state;
  }
}

export type ConnState = "connecting" | "live" | "reconnecting";

export type UseBoard = {
  state: BoardState;
  conn: ConnState;
  name: string;
};

// Every connection is authenticated and bounded. A fresh stream carries a full
// snapshot; reconnect also re-seeds state to cover any gap before the stream.
export function useBoardState(slug: string): UseBoard {
  const [state, dispatch] = useReducer(reducer, undefined, initialState);
  const [conn, setConn] = useState<ConnState>("connecting");
  const [name, setName] = useState<string>("");
  const hadOpenedRef = useRef(false);

  useEffect(() => {
    const controller = new AbortController();
    hadOpenedRef.current = false;
    const seed = async () => {
      try {
        const snap = await fetchState(slug, controller.signal);
        if (controller.signal.aborted) return;
        setName(snap.name || slug);
        dispatch({ type: "SNAPSHOT", snapshot: snap });
      } catch { /* The stream snapshot can seed us if this read fails. */ }
    };
    void seed();
    void streamBoardEvents(eventsUrl(slug), {
      signal: controller.signal,
      open: () => {
        if (controller.signal.aborted) return;
        if (hadOpenedRef.current) void seed();
        hadOpenedRef.current = true;
        setConn("live");
      },
      reconnecting: () => { if (!controller.signal.aborted) setConn(hadOpenedRef.current ? "reconnecting" : "connecting"); },
      consume: ({ event, data: encoded }) => {
        if (controller.signal.aborted) return;
        try {
          const data = JSON.parse(encoded);
          switch (event) {
            case "snapshot": setName(data.name || slug); dispatch({ type: "SNAPSHOT", snapshot: data }); break;
            case "node.added": case "node.updated": dispatch({ type: "NODE_UPSERT", node: data }); break;
            case "node.removed": {
              const id = typeof data === "string" ? data : data.id;
              if (id) dispatch({ type: "NODE_REMOVE", id });
              break;
            }
            case "edge.added": dispatch({ type: "EDGE_UPSERT", edge: data }); break;
            case "edge.removed": if (data?.from && data?.to) dispatch({ type: "EDGE_REMOVE", from: data.from, to: data.to }); break;
            case "memory.updated": dispatch({ type: "MEMORY", memory: Array.isArray(data) ? data : data.memory ?? [] }); break;
            case "activity.updated": dispatch({ type: "ACTIVITY", activity: Array.isArray(data) ? data : data.activity ?? [] }); break;
          }
        } catch { /* Malformed bounded frames never become HTML or control input. */ }
      },
    });
    return () => controller.abort();
  }, [slug]);

  return { state, conn, name };
}
