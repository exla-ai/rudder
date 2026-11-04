import os from 'node:os';
import path from 'node:path';
import { existsSync } from 'node:fs';

// Large Linux fixtures stay on workspace storage. Macs have no /workspace;
// callers own and remove the entire directory returned by mkdtemp(prefix).
export function v4ScratchPrefix(name, platform = process.platform, workspaceExists = existsSync('/workspace'), temporary = os.tmpdir()) {
  return path.join(platform === 'linux' && workspaceExists ? '/workspace' : temporary, `rudder-${name}-`);
}
