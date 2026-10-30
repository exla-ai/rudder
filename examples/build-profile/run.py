#!/usr/bin/env python3
"""Run the opt-in real build/cache workload and leave raw evidence on disk."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import uuid

parser = argparse.ArgumentParser()
parser.add_argument("--directory", type=Path, required=True, help="owned directory on a large workspace volume")
parser.add_argument("--debug-engine", action="store_true", help="use the development Rust engine; timings include slower development hashing")
args = parser.parse_args()
root = args.directory.resolve()
if str(root).startswith("/tmp/") or str(root) == "/tmp":
    raise ValueError("large build fixtures must be outside the worker's small /tmp")
root.mkdir(parents=True, exist_ok=True)
if shutil.disk_usage(root).free < 5 * 1024 * 1024 * 1024:
    raise ValueError("this workload needs at least 5 GiB free")
repo = Path(__file__).resolve().parents[2]
subprocess.run(["python3", str(Path(__file__).with_name("prepare_tools.py")), "--directory", str(root / "toolchain")], cwd=repo, check=True)
workload = root / ("run-" + uuid.uuid4().hex)
environment = {**os.environ, "RUDDER_BUILD_TOOLS_JSON": str(root / "toolchain/toolchain.json"), "RUDDER_BUILD_WORKLOAD_ROOT": str(workload)}
command = ["cargo", "test", *([] if args.debug_engine else ["--release"]), "-p", "rudder-engine", "--test", "build_workload", "--", "--exact", "qualified_rust_typescript_cache_workload", "--ignored", "--nocapture"]
subprocess.run(command, cwd=repo, env=environment, check=True)
print(json.dumps({"summary": str(workload / "summary.json"), "raw_evidence": str(workload / "evidence")}, indent=2))
