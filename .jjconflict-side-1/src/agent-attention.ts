export type AgentAttention = {
  needsPermission: boolean;
  summary?: string;
};

const PERMISSION_WORD = /\b(permission|approval|approve|allow|authorize|authorization|confirmation|proceed|deny)\b/i;
const PERMISSION_PATTERNS = [
  /\b(do you want|would you like|are you sure)\b[\s\S]{0,180}\b(allow|approve|run|execute|continue|proceed)\b/i,
  /\b(allow|approve|authorize)\b[\s\S]{0,120}\b(command|tool|edit|write|file|access|execution|network|shell|operation)\b/i,
  /\b(permission|approval|authorization|confirmation)\b[\s\S]{0,160}\b(required|need(?:ed)?|request(?:ed|ing)?|waiting|prompt)\b[\s\S]{0,160}\b(approve|allow|deny|yes|no|enter|return|press)\b/i,
  /\b(approve|allow|deny|yes|no|enter|return|press)\b[\s\S]{0,160}\b(permission|approval|authorization|confirmation)\b/i,
  /\bpress\b[\s\S]{0,80}\b(y|yes|enter|return)\b[\s\S]{0,120}\b(allow|approve|continue|proceed)\b/i,
  /\b(yes|no)\b[\s\S]{0,100}\b(approve|deny|allow|permission)\b/i,
];

export function permissionAttentionFromOutput(output: string): AgentAttention {
  const recentLines = recentTerminalLines(output, 60);
  const recentText = recentLines.join("\n");
  if (!PERMISSION_WORD.test(recentText)) {
    return { needsPermission: false };
  }
  if (!PERMISSION_PATTERNS.some((pattern) => pattern.test(recentText))) {
    return { needsPermission: false };
  }
  return {
    needsPermission: true,
    summary: summarizePermissionPrompt(recentLines),
  };
}

function summarizePermissionPrompt(lines: string[]): string | undefined {
  const line = [...lines]
    .reverse()
    .find((candidate) => PERMISSION_WORD.test(candidate));
  if (!line) {
    return undefined;
  }
  return truncate(line.replace(/\s+/g, " ").trim(), 120);
}

function recentTerminalLines(output: string, maxLines: number): string[] {
  return stripTerminalControls(output)
    .replace(/\r/g, "\n")
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .slice(-maxLines);
}

function stripTerminalControls(value: string): string {
  return value
    .replace(/\x1b\][^\u0007]*(?:\u0007|\x1b\\)/g, "")
    .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "")
    .replace(/\x1b[()][A-Za-z0-9]/g, "")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, "");
}

function truncate(value: string, width: number): string {
  if (value.length <= width) {
    return value;
  }
  return `${value.slice(0, Math.max(0, width - 3))}...`;
}
