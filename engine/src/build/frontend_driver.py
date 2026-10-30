#!/usr/bin/env python3
"""Managed Buck2 frontend adapter. Input is trusted engine JSON on stdin.

The engine supplies CAS-materialized inputs/tools and writes this bundled
driver plus linux_runner.py. No user BUILD/Starlark/shell program is loaded.
"""
import gzip
import hashlib
import json
import os
import shutil
from pathlib import Path
import socket
import subprocess
import sys
import time
import uuid


def write_if_changed(path, text):
    if not path.exists() or path.read_text() != text:
        path.write_text(text)


def rules(remote, platform):
    properties = {"OSFamily": platform["os"], "Arch": platform["architecture"], "rudder-abi": platform["abi"], **platform.get("properties", {})}
    return '''def _compile(ctx):
    output = ctx.actions.declare_output("js" if ctx.attrs.language == "typescript" else ctx.label.name + ".rlib", dir = ctx.attrs.language == "typescript")
    loader = "prepared/lib64/ld-linux-x86-64.so.2"
    libraries = "prepared/lib/x86_64-linux-gnu:prepared/usr/lib/x86_64-linux-gnu"
    argv = cmd_args([loader, "--library-path", libraries, "prepared/tools/unshare", "--user", "--map-root-user", "--mount", "--net", "--pid", "--fork", "--kill-child", loader, "--library-path", libraries, "prepared/tools/python/bin/python3", "-s", "-P", ctx.attrs.runner, "--prepared", "prepared", "--kind", ctx.attrs.language, "--entrypoint", ctx.attrs.entrypoint, "--output", output.as_output(), "--environment", ctx.attrs.environment])
    for source in ctx.attrs.sources:
        argv.add("--input", source)
    argv.add(cmd_args(hidden = [ctx.attrs.contract]))
    ctx.actions.run(argv, category = "rudder_compile", identifier = ctx.attrs.language, local_only = True, allow_cache_upload = True, env = {"PYTHONHOME": "prepared/tools/python", "PYTHONDONTWRITEBYTECODE": "1", "PYTHONHASHSEED": "0"})
    return [DefaultInfo(default_output = output)]
compile_target = rule(impl = _compile, attrs = {"language": attrs.string(), "entrypoint": attrs.source(), "sources": attrs.list(attrs.source()), "runner": attrs.source(), "contract": attrs.source(), "environment": attrs.string()})
def _platforms(ctx):
    executor = CommandExecutorConfig(local_enabled = True, remote_enabled = False, remote_cache_enabled = REMOTE, allow_cache_uploads = REMOTE, remote_execution_properties = PROPERTIES, remote_execution_use_case = "buck2-default")
    platform = ExecutionPlatformInfo(label = ctx.label.raw_target(), configuration = ConfigurationInfo(constraints = {}, values = {}), executor_config = executor)
    return [DefaultInfo(), ExecutionPlatformRegistrationInfo(platforms = [platform])]
platforms = rule(impl = _platforms, attrs = {})
'''.replace("REMOTE", "True" if remote else "False").replace("PROPERTIES", json.dumps(properties, sort_keys=True))


def parse_events(path):
    executed, hits, digests = 0, 0, set()
    with gzip.open(path, "rt") as stream:
        for line in stream:
            event = json.loads(line).get("Event", {}).get("data", {}).get("SpanEnd", {}).get("data", {}).get("ActionExecution")
            if not event or event.get("kind") != "Run" or event.get("name", {}).get("category") != "rudder_compile":
                continue
            if event.get("failed"):
                raise ValueError("failed compiler action cannot publish a successful build")
            local, cached = False, False
            for command in event.get("commands", []):
                details = command.get("details", {})
                variant = details.get("command_kind", {}).get("command", {})
                for name, value in variant.items():
                    raw = value.get("action_digest")
                    if raw:
                        digest, size = raw.rsplit(":", 1)
                        if len(digest) != 64 or not all(c in "0123456789abcdef" for c in digest):
                            raise ValueError("frontend did not report a SHA-256 action digest")
                        digests.add((digest, int(size)))
                    if name in ("LocalCommand", "OmittedLocalCommand") and "Success" in command.get("status", {}):
                        local = True
                    if name == "RemoteCommand" and value.get("cache_hit"):
                        cached = True
            executed += int(local)
            hits += int(cached)
    return executed, hits, [{"hash": digest, "size_bytes": size} for digest, size in sorted(digests)]


def main():
    request = json.load(sys.stdin)
    if request.get("format_version") != 1:
        raise ValueError("unsupported managed frontend request")
    workspace = Path(request["workspace"]).resolve()
    prepared = Path(request["prepared_directory"]).resolve(strict=True)
    buck2 = Path(request["buck2"]).resolve(strict=True)
    profile = request["profile"]
    if profile["policy"].get("effects"):
        raise ValueError("effectful steps are outside this cacheable compile frontend")
    if hashlib.sha256(buck2.read_bytes()).hexdigest() != profile["buck2"]["executable_sha256"]:
        raise ValueError("Buck2 executable pin mismatch")
    workspace.mkdir(parents=True, exist_ok=True)
    os.chdir(workspace)
    raw = workspace / ".driver" / str(uuid.uuid4())
    raw.mkdir(parents=True)
    remote = profile["buck2"].get("remote_cache")
    config = "[cells]\nroot = .\n[project]\nignore = prepared, .driver\n[build]\nexecution_platforms = root//:platforms\n"
    if remote:
        config += "[buck2_re_client]\n" + "\n".join(f"{key} = {remote[key]}" for key in ("cas_address", "action_cache_address", "instance_name")) + f"\nengine_address = {remote['action_cache_address']}\ntls = {str(remote['tls']).lower()}\n"
    write_if_changed(workspace / ".buckconfig", config)
    write_if_changed(workspace / ".buckroot", "")
    write_if_changed(workspace / "rules.bzl", rules(bool(remote), profile["platform"]))
    contract = request["native_contract"]
    write_if_changed(workspace / "contract.json", json.dumps(contract, sort_keys=True, separators=(",", ":")))
    link = workspace / "prepared"
    if link.is_symlink():
        if link.resolve() != prepared:
            link.unlink()
    elif link.exists():
        raise ValueError("prepared environment mountpoint must be engine-owned")
    if not link.is_symlink():
        link.symlink_to(prepared, target_is_directory=True)
    lines = ['load(":rules.bzl", "compile_target", "platforms")', 'platforms(name = "platforms")']
    for target in profile["targets"]:
        sources = ["inputs/" + value for value in sorted(target["sources"])]
        lines.append("compile_target(name = %s, language = %s, entrypoint = %s, sources = %s, environment = %s, runner = \"linux_runner.py\", contract = \"contract.json\")" % (json.dumps(target["name"]), json.dumps(target["language"]), json.dumps("inputs/" + target["entrypoint"]), json.dumps(sources), json.dumps(json.dumps(profile["policy"].get("environment", {}), sort_keys=True))))
    write_if_changed(workspace / "BUCK", "\n".join(lines) + "\n")
    environment = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": str(workspace / ".driver" / "home"), "TMPDIR": str(workspace / ".driver" / "tmp"), "LANG": "C", "LC_ALL": "C"}
    Path(environment["HOME"]).mkdir(exist_ok=True)
    Path(environment["TMPDIR"]).mkdir(exist_ok=True)
    # A listening host socket proves the negative network probe is meaningful.
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(4)
        port = listener.getsockname()[1]
        with socket.create_connection(("127.0.0.1", port), timeout=1):
            pass
        probe_output = "buck-out/rudder-probe/qualification.json"
        loader = "prepared/lib64/ld-linux-x86-64.so.2"
        libraries = "prepared/lib/x86_64-linux-gnu:prepared/usr/lib/x86_64-linux-gnu"
        probe_env = {**environment, "PYTHONHOME": "prepared/tools/python", "PYTHONDONTWRITEBYTECODE": "1", "PYTHONHASHSEED": "0"}
        probe = subprocess.run([loader, "--library-path", libraries, "prepared/tools/unshare", "--user", "--map-root-user", "--mount", "--net", "--pid", "--fork", "--kill-child", loader, "--library-path", libraries, "prepared/tools/python/bin/python3", "-s", "-P", "linux_runner.py", "--prepared", "prepared", "--kind", "probe", "--output", probe_output, "--probe-port", str(port)], env=probe_env, capture_output=True, text=True, timeout=10)
        if probe.returncode:
            raise ValueError("Linux compile sandbox qualification failed: " + probe.stderr[-2000:])
        qualification = json.loads(Path(probe_output).read_text())
        if qualification != {"host_files_visible": False, "host_network_reachable": False}:
            raise ValueError("Linux compiler isolation did not qualify")
    report, events = raw / "report.json", raw / "events.json"
    command = [str(buck2), "build", *["//:" + target["name"] for target in profile["targets"]], "--console", "simple", "--build-report", str(report), "--event-log", str(events)]
    started = time.perf_counter()
    with (raw / "stdout.log").open("wb") as stdout, (raw / "stderr.log").open("wb") as stderr:
        try:
            process = subprocess.run(command, env=environment, stdout=stdout, stderr=stderr, timeout=120)
        except subprocess.TimeoutExpired:
            subprocess.run([str(buck2), "kill"], env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
            raise ValueError("managed build timed out; no result published")
    elapsed_ms = int((time.perf_counter() - started) * 1000)
    build_report = json.loads(report.read_text())
    if process.returncode or not build_report.get("success"):
        raise ValueError("managed build failed; no result published: " + (raw / "stderr.log").read_text()[-5000:])
    shutil.rmtree(workspace / "buck-out" / "rudder-scratch", ignore_errors=False)
    executed, hits, digests = parse_events(events)
    outputs = {}
    for target in profile["targets"]:
        value = build_report["results"]["root//:" + target["name"]]
        paths = value["outputs"]["DEFAULT"]
        if len(paths) != 1 or not Path(paths[0]).resolve().is_relative_to(workspace / "buck-out"):
            raise ValueError("frontend output escaped its managed target")
        outputs[target["name"]] = paths[0]
    print(json.dumps({"format_version": 1, "trace_id": build_report["trace_id"], "executed_actions": executed, "remote_cache_hits": hits,
                      "graph_reused_actions": len(profile["targets"]) - executed - hits, "native_action_digests": digests,
                      "outputs": outputs, "event_log": str(events), "build_report": str(report), "elapsed_ms": elapsed_ms,
                      "sandbox_qualification": qualification}, sort_keys=True))


if __name__ == "__main__":
    main()
