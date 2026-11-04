#!/usr/bin/env node
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

type CwdRecovery = {
  recovered: boolean;
  original?: string;
  current?: string;
  error?: unknown;
};

const recovery = recoverCwdIfNeeded();

try {
  const { main } = await import("./main.js");
  if (recovery.recovered) {
    console.error(formatCwdRecoveryNotice(recovery));
  }
  await main();
} catch (error: unknown) {
  if (isMissingToolError(error)) {
    console.error(error.message);
    process.exit(1);
  }
  const message = error instanceof Error ? error.message : String(error);
  console.error(`rudder: ${message}`);
  process.exit(1);
}

function recoverCwdIfNeeded(): CwdRecovery {
  try {
    return { recovered: false, current: process.cwd() };
  } catch (error) {
    const original = process.env.PWD?.trim() || undefined;
    process.env.RUDDER_ORIGINAL_CWD = original ?? "";
    process.env.RUDDER_CWD_RECOVERY_ERROR = error instanceof Error ? error.message : String(error);

    for (const candidate of cwdRecoveryCandidates(original)) {
      try {
        if (!isReadableDirectory(candidate)) {
          continue;
        }
        process.chdir(candidate);
        const current = process.cwd();
        process.env.RUDDER_RECOVERED_CWD = current;
        return { recovered: true, original, current, error };
      } catch {
        // Try the next fallback candidate.
      }
    }

    console.error(
      [
        "rudder: current directory is not readable, and Rudder could not recover.",
        original ? `  original cwd: ${original}` : undefined,
        "  Try `cd ~` or another readable directory, then run `rudder --cwd <repo>`.",
      ]
        .filter(Boolean)
        .join("\n"),
    );
    process.exit(1);
  }
}

function cwdRecoveryCandidates(original: string | undefined): string[] {
  const candidates: string[] = [];
  if (original) {
    candidates.push(original);
    let next = path.dirname(original);
    while (next && next !== path.dirname(next)) {
      candidates.push(next);
      next = path.dirname(next);
    }
  }
  candidates.push(os.homedir(), "/tmp", "/");
  return [...new Set(candidates.filter(Boolean))];
}

function isReadableDirectory(candidate: string): boolean {
  try {
    const stat = fs.statSync(candidate);
    if (!stat.isDirectory()) {
      return false;
    }
    fs.accessSync(candidate, fs.constants.R_OK | fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function formatCwdRecoveryNotice(recovery: CwdRecovery): string {
  const original = recovery.original ? ` from ${recovery.original}` : "";
  const current = recovery.current ?? process.cwd();
  return `rudder: recovered from an unreadable cwd${original}; using ${current}`;
}

function isMissingToolError(error: unknown): error is Error {
  return error instanceof Error && error.name === "MissingToolError";
}
