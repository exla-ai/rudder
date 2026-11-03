#!/usr/bin/env python3
"""Build one committed managed worker + pinned C1 image, without deployment.

All commands run against a new git archive, never the working tree's dist.
Build output is private; a successful build is not runtime/SDK qualification.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import signal
import stat
import subprocess
import sys
import tarfile

C1_IMAGE = "registry.fly.io/rudder-workers-exla@sha256:19ce471b0c894dcd820135d1d97d2a1f5a355938cd339acedd8373efa86e070e"
C1_PINS = {
    "inventory.json": "4f8d57a0ffd78335a9ca9863d26040379d65dc8942fa32d41e217f898612f832",
    "manifest.json": "d60afb84b6e373fece5fd2a0babb1b66bd0f4c637efddf766f59a180e39ec54a",
    "compiler-source.json": "301ec13415fa57e2623571adce9327b4e406d558698a1fa46adfb7e04f5fbbc0",
}
MARKER = "RUDDER_MANAGED_C1_PRODUCT_DATA_V1="
MANAGED_PATHS = (
    "/opt/rudder/dist/native/rudder-native", "/opt/rudder/dist/native/rudder-jj",
    "/usr/local/bin/rudder-root-storage", "/usr/local/bin/rudder-machine-root",
    "/var/lib/rudder-image-cas-v1/inventory.json",
)
METADATA_PATHS = (
    "/opt/rudder-managed-runtime/v1/manifest.json", "/opt/rudder-host-control/manifest.json",
    "/usr/local/share/rudder-managed-image/policy.json", "/usr/local/share/rudder-managed-image/components.json",
    "/usr/local/share/rudder-managed-image/runtime-config-pin.json",
    "/usr/local/share/rudder-managed-image/sidecar-policy-pin.json",
    "/var/lib/rudder-image-cas-v1/manifest.json", "/var/lib/rudder-image-cas-v1/pin.json",
)

FLOOR = 3 * 1024**3
HELD = []

# The read-only mount is the SAME build's managed stage. No old base receipt,
# expected hash argument or mutable registry tag supplies the before bytes.
VERIFY_C1 = r'''
import hashlib,json,os,stat
from pathlib import Path
def sha(path):
    h=hashlib.sha256()
    with open(path,'rb') as f:
        while True:
            b=f.read(1024*1024)
            if not b:break
            h.update(b)
    return h.hexdigest()
paths=['/opt/rudder/dist/native/rudder-native','/opt/rudder/dist/native/rudder-jj','/usr/local/bin/rudder-root-storage','/usr/local/bin/rudder-machine-root','/var/lib/rudder-image-cas-v1/inventory.json']
observed={}
for name in paths:
    p=Path(name);s=p.lstat()
    assert stat.S_ISREG(s.st_mode) and s.st_uid==s.st_gid==0 and s.st_nlink==1 and not s.st_mode&0o022
    before=Path('/rudder-original'+name)
    assert sha(p)==sha(before) and s.st_size==before.stat().st_size and stat.S_IMODE(s.st_mode)==stat.S_IMODE(before.stat().st_mode),(name,'same-build managed bytes changed')
    observed[name]={'sha256':sha(p),'bytes':s.st_size}
metadata={}
for name in ['/opt/rudder-managed-runtime/v1/manifest.json','/opt/rudder-host-control/manifest.json','/usr/local/share/rudder-managed-image/policy.json','/usr/local/share/rudder-managed-image/components.json','/usr/local/share/rudder-managed-image/runtime-config-pin.json','/usr/local/share/rudder-managed-image/sidecar-policy-pin.json','/var/lib/rudder-image-cas-v1/manifest.json','/var/lib/rudder-image-cas-v1/pin.json']:
    p=Path(name);before=Path('/rudder-original'+name);s=p.lstat()
    assert stat.S_ISREG(s.st_mode) and s.st_uid==s.st_gid==0 and s.st_nlink==1 and not s.st_mode&0o022
    assert sha(p)==sha(before) and s.st_size==before.stat().st_size and stat.S_IMODE(s.st_mode)==stat.S_IMODE(before.stat().st_mode)
    metadata[name]={'sha256':sha(p),'bytes':p.stat().st_size}
runtime=json.loads(Path('/var/lib/rudder-image-cas-v1/inventory.json').read_bytes())
assert json.loads(Path('/var/lib/rudder-image-cas-v1/pin.json').read_bytes())==runtime['roots']
base=Path('/var/lib/rudder-compiler-c1-cas-v1')
pins=__C1_PINS__
for name,expected in pins.items():
    p=base/name;s=p.lstat()
    assert stat.S_ISREG(s.st_mode) and s.st_uid==s.st_gid==0 and s.st_nlink==1 and stat.S_IMODE(s.st_mode)==0o444 and sha(p)==expected
inv=json.loads((base/'inventory.json').read_bytes())
assert len(inv['objects'])==6430 and inv['total_bytes']==1174586558
assert json.loads((base/'pin.json').read_bytes())==inv['roots']
for path in [base,base/'cas',Path('/opt/rudder-worker/compiler-sdk-v1')]:
    for ancestor in [path,*path.parents]:
        s=ancestor.lstat()
        assert stat.S_ISDIR(s.st_mode) and s.st_uid==s.st_gid==0 and not s.st_mode&0o022
assert stat.S_IMODE(base.stat().st_mode)==0o555
assert stat.S_IMODE(Path('/opt/rudder-worker').stat().st_mode)==0o555
placeholder=Path('/opt/rudder-worker/compiler-sdk-v1')
assert stat.S_IMODE(placeholder.stat().st_mode)==0o555 and not any(placeholder.iterdir())
# Existing descriptor/layout verification is repeated after C1 publication.
def bounded_json(name):
    raw=Path(name).read_bytes()
    assert 0<len(raw)<=65536
    return json.loads(raw)
def inventory_descriptor(name):
    return {'kind':'blob','algorithm':'sha256','digest':sha(Path(name)),'size_bytes':Path(name).stat().st_size}
product={'managedFiles':observed,'managedMetadata':metadata,'runtimeRoots':runtime['roots'],
         'runtimeManifest':bounded_json('/usr/local/share/rudder-managed-image/runtime-config-pin.json'),
         'sidecarPolicy':bounded_json('/usr/local/share/rudder-managed-image/sidecar-policy-pin.json'),
         'components':bounded_json('/usr/local/share/rudder-managed-image/components.json'),
         'runtimeData':{'layout':'baked-image-cas-v1','inventory':inventory_descriptor('/var/lib/rudder-image-cas-v1/inventory.json'),
                        'roots':runtime['roots'],'total_bytes':runtime['total_bytes']},
         'compilerC1':pins,'compilerC1Roots':inv['roots'],
         'compilerC1Data':{'inventory':inventory_descriptor(str(base/'inventory.json')),'roots':inv['roots'],'total_bytes':inv['total_bytes']},
         'sdkQualified':False,'runtimeQualified':False}
assert product['runtimeManifest']['manifest_sha256']==metadata['/opt/rudder-managed-runtime/v1/manifest.json']['sha256']
encoded=json.dumps(product,sort_keys=True,separators=(',',':'))
assert len(encoded.encode())<=60000
print('__MARKER__'+encoded)
'''


def need(condition, message):
    if not condition:
        raise ValueError(message)


def sha_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while block := f.read(1024 * 1024):
            h.update(block)
    return h.hexdigest()


def sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def write_new(path, raw):
    with open(path, "xb") as f:
        os.fchmod(f.fileno(), 0o600)
        f.write(raw)
        f.flush()
        os.fsync(f.fileno())
    sync_dir(path.parent)


def json_bytes(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def disk_check(path):
    need(shutil.disk_usage(path).free >= FLOOR, "less than 3 GiB available before stage")


def hold():
    # Preserve this exact possibly attempted original child and its open log.
    # There is no retry, new child, success projection or remote-cancel claim.
    while True:
        try:
            signal.pause()
        except BaseException:
            pass


class OriginalCommand:
    def __init__(self, output, name, args, cwd, env, stdout_path=None):
        self.args, self.cwd, self.env = args, cwd, env
        self.name = name
        self.log_path = output / (name + ".log")
        self.stdout_path = stdout_path
        self.child = None
        self.pidfd = None
        self.code = None
        self.log = None
        self.stdout = None
        HELD.append(self)

    def run(self):
        disk_check(self.cwd)
        self.log = open(self.log_path, "xb")
        os.fchmod(self.log.fileno(), 0o600)
        if self.stdout_path:
            self.stdout = open(self.stdout_path, "xb")
            os.fchmod(self.stdout.fileno(), 0o600)
        sync_dir(self.log_path.parent)
        # Allocate the carrier before the possibly attempted constructor.
        self.child = subprocess.Popen.__new__(subprocess.Popen)
        try:
            self.child.__init__(self.args, cwd=self.cwd, env=self.env,
                                stdin=subprocess.DEVNULL,
                                stdout=self.stdout or self.log, stderr=self.log)
            if hasattr(os, "pidfd_open"):
                self.pidfd = os.pidfd_open(self.child.pid)
            self.code = self.child.wait()
            need(type(self.code) is int, "original wait type unknown")
            for f in (self.stdout, self.log):
                if f:
                    f.flush()
                    os.fsync(f.fileno())
            sync_dir(self.log_path.parent)
            row = {"stage": self.name, "argv": self.args, "pid": self.child.pid,
                   "actualWait": True, "exitCode": self.code,
                   "pidfdHeld": self.pidfd is not None,
                   "fileBackedLog": {"path": str(self.log_path), "sha256": sha_file(self.log_path),
                                     "bytes": self.log_path.stat().st_size},
                   "pipeEofClaimed": False}
            write_new(self.log_path.with_suffix(".json"), json_bytes(row))
        except BaseException:
            print("original command uncertainty; retained stage=" + self.name, file=sys.stderr, flush=True)
            hold()
        finally:
            # A known actual wait and durable stage record precede closure.
            if type(self.code) is int:
                if self.pidfd is not None:
                    os.close(self.pidfd)
                    self.pidfd = None
                if self.stdout:
                    self.stdout.close()
                self.log.close()
        need(self.code == 0, f"stage {self.name} failed (exit {self.code}); retained log {self.log_path}")
        return row


def render_dockerfile(original, revision):
    need(re.fullmatch(r"[a-f0-9]{40}", revision), "expected full committed revision")
    need(original.count(" AS managed\n") == 1, "managed target missing or ambiguous")
    verify = VERIFY_C1.replace("__C1_PINS__", repr(C1_PINS)).replace("__MARKER__", MARKER)
    return original + f'''

# One-revision candidate: stock managed bake + immutable C1 data only.
FROM {C1_IMAGE} AS compiler_c1_data
FROM managed AS managed_c1
USER root
RUN test -d /var/lib/rudder-image-cas-v1 \\
 && test ! -e /var/lib/rudder-compiler-c1-cas-v1 \\
 && test ! -L /var/lib/rudder-compiler-c1-cas-v1
COPY --from=compiler_c1_data --chown=0:0 /var/lib/rudder-compiler-c1-cas-v1/ /var/lib/rudder-compiler-c1-cas-v1/
RUN chmod 0555 /var/lib/rudder-compiler-c1-cas-v1
ARG RUDDER_CANDIDATE_BUILD_ID
RUN --mount=type=bind,from=managed,source=/,target=/rudder-original,ro test -n "$RUDDER_CANDIDATE_BUILD_ID" && /usr/bin/python3 -I -B - <<'PY'
{verify}
PY
RUN /usr/bin/python3 -I -B /usr/local/libexec/rudder-managed-image-layout
LABEL org.opencontainers.image.revision="{revision}" \\
      ai.exla.rudder.compiler-packaging="c1-a6-cas-data-only"
# Inherit the managed env-i machine-root ENTRYPOINT and empty CMD.
'''


def extract_archive(archive, destination):
    with tarfile.open(archive, "r:") as tar:
        names = set()
        for member in tar.getmembers():
            p = PurePosixPath(member.name)
            need(not p.is_absolute() and ".." not in p.parts and member.name not in names,
                 "unsafe or duplicate archive member")
            need(member.isfile() or member.isdir(), "archive links/devices are unsupported")
            names.add(member.name)
        # git archive has no dependency directories or generated output.
        need(not any(n == "node_modules" or n.startswith(("node_modules/", "cloud/node_modules/", "dist/", "cloud/dist/")) for n in names), "archive contains generated/dependency output")
        # Older Python 3.11 lacks extraction filters. Every member above is
        # already restricted to regular files/directories in a fresh empty root.
        if hasattr(tarfile, "data_filter"):
            tar.extractall(destination, filter="data")
        else:
            tar.extractall(destination)
        # git archive defaults to group-writable modes. Older Python's fallback
        # does not apply data_filter's mode stripping, and implicit parents can
        # inherit our private umask. Normalize only captured source, retaining
        # its private root; bytes and executable intent remain unchanged.
        for path in destination.rglob("*"):
            s = path.lstat()
            need(stat.S_ISDIR(s.st_mode) or stat.S_ISREG(s.st_mode), "invalid extracted source")
            path.chmod(0o755 if stat.S_ISDIR(s.st_mode) or s.st_mode & 0o111 else 0o644)


def generated_files(source):
    js, assets = {}, {}
    for root in (source / "dist", source / "cloud/dist"):
        need(root.is_dir(), "generated distribution missing")
        for path in sorted(root.rglob("*")):
            if path.is_dir():
                continue
            need(path.is_file() and not path.is_symlink(), "generated output must be regular")
            row = {"sha256": sha_file(path), "bytes": path.stat().st_size}
            (js if path.suffix == ".js" else assets)[path.relative_to(source).as_posix()] = row
    need("dist/index.js" in js and "cloud/dist/managed-home-provisioning/staging-sdk-runtime-flow.js" in js,
         "required generated JS missing")
    return js, assets


def parse_build_lines(lines, image):
    pushes, product, traditional, summaries = [], [], [], []
    ansi = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
    prefix = r"(?:#[0-9]{1,6}[ \t]+(?:[0-9]{1,9}\.[0-9]{1,6}[ \t]+)?)?"
    duration = r"[0-9]{1,6}(?:\.[0-9]{1,6})?s"
    pattern = prefix + "pushing manifest for " + re.escape(image) + r"@(sha256:[a-f0-9]{64})(?:[ \t]+" + duration + r")?(?:[ \t]+(done)(?:[ \t]+" + duration + r")?)?[ \t]*"
    for number, raw in enumerate(lines):
        need(len(raw) <= 65536, "oversized build log line")
        line = ansi.sub("", raw.decode("utf8", "strict")).rstrip("\r\n")
        if "pushing manifest for " in line:
            match = re.fullmatch(pattern, line)
            need(match is not None, "unexpected manifest push output")
            pushes.append((match[1], match[2] == "done"))
        if ": digest:" in line:
            # Traditional Fly builds report the tag here, then complete the
            # push and print the full image. Bind all three from this one log.
            tag = image.rsplit("/", 1)[-1].partition(":")[2]
            match = re.fullmatch(re.escape(tag) + r": digest: (sha256:[a-f0-9]{64}) size: [1-9][0-9]{0,18}", line)
            need(tag and match is not None, "unexpected traditional push digest")
            traditional.append(("digest", match[1], number))
        if "Pushing image done" in line:
            need(line == "--> Pushing image done", "unexpected traditional push completion")
            traditional.append(("done", None, number))
        if line.startswith("image:"):
            need(line == "image: " + image, "unexpected pushed image summary")
            summaries.append(number)
        match = re.fullmatch(prefix + re.escape(MARKER) + r"(\{.*\})", line)
        if match:
            product.append(json.loads(match[1]))
    if traditional:
        need(not pushes and [kind for kind, _, _ in traditional] == ["digest", "done"]
             and len(summaries) == 1 and summaries[0] > traditional[-1][2],
             "one completed traditional image push required without mixed output")
        digest = traditional[0][1]
    else:
        need(pushes and pushes[-1][1] and sum(done for _, done in pushes) == 1 and len({d for d, _ in pushes}) == 1,
             "one completed immutable image push required")
        digest = pushes[-1][0]
    need(len(product) == 1 and product[0].get("compilerC1") == C1_PINS and product[0].get("sdkQualified") is False and product[0].get("runtimeQualified") is False,
         "same-build managed/C1 product marker missing or ambiguous")
    files = product[0].get("managedFiles")
    need(type(files) is dict and set(files) == set(MANAGED_PATHS), "closed managed product inventory required")
    for row in files.values():
        need(type(row) is dict and set(row) == {"sha256", "bytes"}
             and type(row["sha256"]) is str and re.fullmatch(r"[a-f0-9]{64}", row["sha256"])
             and type(row["bytes"]) is int and row["bytes"] > 0, "invalid managed product row")
    value = product[0]
    need(set(value) == {"managedFiles", "managedMetadata", "runtimeRoots", "runtimeManifest", "sidecarPolicy", "components",
                        "runtimeData", "compilerC1", "compilerC1Roots", "compilerC1Data", "sdkQualified", "runtimeQualified"},
         "closed same-build product data required")
    metadata = value["managedMetadata"]
    need(type(metadata) is dict and set(metadata) == set(METADATA_PATHS), "closed managed metadata inventory required")
    for row in metadata.values():
        need(type(row) is dict and set(row) == {"sha256", "bytes"} and type(row["sha256"]) is str
             and re.fullmatch(r"[a-f0-9]{64}", row["sha256"]) and type(row["bytes"]) is int and row["bytes"] > 0,
             "invalid managed metadata row")
    manifest = value["runtimeManifest"]
    need(type(manifest) is dict and set(manifest) == {"profile_id", "manifest_sha256", "entries"}
         and manifest["profile_id"] == "fly-managed-image-v1" and type(manifest["entries"]) is list and 0 < len(manifest["entries"]) <= 64
         and manifest["manifest_sha256"] == metadata["/opt/rudder-managed-runtime/v1/manifest.json"]["sha256"],
         "actual runtime manifest required")
    need(all(type(value[key]) is dict and value[key] for key in ("sidecarPolicy", "components", "runtimeRoots", "compilerC1Roots")),
         "actual managed policy components and roots required")
    runtime = value["runtimeData"]
    inventory = files["/var/lib/rudder-image-cas-v1/inventory.json"]
    need(type(runtime) is dict and set(runtime) == {"layout", "inventory", "roots", "total_bytes"}
         and runtime["layout"] == "baked-image-cas-v1" and runtime["roots"] == value["runtimeRoots"]
         and runtime["inventory"] == dict(kind="blob", algorithm="sha256", digest=inventory["sha256"], size_bytes=inventory["bytes"])
         and type(runtime["total_bytes"]) is int and runtime["total_bytes"] > 0, "actual runtime CAS descriptor required")
    c1 = value["compilerC1Data"]
    need(type(c1) is dict and set(c1) == {"inventory", "roots", "total_bytes"}
         and c1["roots"] == value["compilerC1Roots"] and type(c1["total_bytes"]) is int and c1["total_bytes"] == 1174586558,
         "actual compiler C1 roots required")
    descriptor = c1["inventory"]
    need(type(descriptor) is dict and set(descriptor) == {"kind", "algorithm", "digest", "size_bytes"}
         and descriptor["kind"] == "blob" and descriptor["algorithm"] == "sha256" and descriptor["digest"] == C1_PINS["inventory.json"]
         and type(descriptor["size_bytes"]) is int and descriptor["size_bytes"] > 0, "actual C1 inventory descriptor required")
    return digest, product[0]


def parse_build_log(path, image):
    with open(path, "rb") as stream:
        return parse_build_lines(stream, image)


def synthetic_product():
    files = {name: {"sha256": "c" * 64, "bytes": 1} for name in MANAGED_PATHS}
    return {"compilerC1": C1_PINS, "sdkQualified": False, "runtimeQualified": False, "managedFiles": files,
            "managedMetadata": {name: {"sha256": "d" * 64, "bytes": 1} for name in METADATA_PATHS},
            "runtimeManifest": {"profile_id": "fly-managed-image-v1", "manifest_sha256": "d" * 64,
                                "entries": [{"path": "/opt/rudder-managed-runtime/v1/example", "sha256": "e" * 64, "size_bytes": 1}]},
            "sidecarPolicy": {"synthetic": True}, "components": {"synthetic": True},
            "runtimeRoots": {"synthetic": True}, "compilerC1Roots": {"synthetic": True},
            "runtimeData": {"layout": "baked-image-cas-v1", "roots": {"synthetic": True}, "total_bytes": 1,
                            "inventory": {"kind": "blob", "algorithm": "sha256", "digest": "c" * 64, "size_bytes": 1}},
            "compilerC1Data": {"roots": {"synthetic": True}, "total_bytes": 1174586558,
                               "inventory": {"kind": "blob", "algorithm": "sha256", "digest": C1_PINS["inventory.json"], "size_bytes": 1}}}


def prepare_generated_modes(source):
    # The enclosing capture/logs stay private. Public build products must still
    # be readable after Docker copies them into a UID1500 worker. tsc inherits
    # our private umask and otherwise emits mode0600 files/mode0700 directories.
    for root in (source / "dist", source / "cloud/dist"):
        for path in (root, *root.rglob("*")):
            s = path.lstat()
            need(not stat.S_ISLNK(s.st_mode), "generated output contains a symlink")
            if stat.S_ISDIR(s.st_mode):
                path.chmod(0o755)
            else:
                need(stat.S_ISREG(s.st_mode) and s.st_nlink == 1, "invalid generated output")
                path.chmod(0o755 if s.st_mode & 0o111 else 0o644)


def validate_worker_payload(source):
    # Cheap local preflight before any remote compiler/image work. These are
    # the source and generated paths copied into the unprivileged worker.
    for root in (source / name for name in ("dist", "assets", "scripts", "cloud/worker")):
        need(root.is_dir(), "worker payload missing")
        for path in (root, *root.rglob("*")):
            s = path.lstat()
            directory = stat.S_ISDIR(s.st_mode)
            need(directory or stat.S_ISREG(s.st_mode), "invalid worker payload file")
            need(not s.st_mode & 0o022 and s.st_mode & (0o555 if directory else 0o444)
                 == (0o555 if directory else 0o444), "worker payload is mutable or unreadable")
    for name in ("package.json", "package-lock.json"):
        s = (source / name).lstat()
        need(stat.S_ISREG(s.st_mode) and not s.st_mode & 0o022 and s.st_mode & 0o444 == 0o444,
             "worker package metadata is mutable or unreadable")
    validate_sidecar_sources(source)


SIDECAR_SOURCE_PINS = {
    "sidecar_script_sha256": "host-control.mjs",
    "sidecar_core_sha256": "host-control-core.mjs",
    "uploader_script_sha256": "root-upload.mjs",
    "qualification_script_sha256": "runtime-qualification.mjs",
    "sidecar_package_sha256": "managed-runtime/sidecar-package.json",
    "sidecar_lock_sha256": "managed-runtime/sidecar-package-lock.json",
}


def validate_sidecar_sources(source):
    # Check immutable source pins before installing dependencies or starting a
    # remote build. Docker repeats this check against the actual image closure.
    worker = source / "cloud/worker"
    policy = json.loads((worker / "managed-runtime/policy.json").read_bytes())
    for field, name in SIDECAR_SOURCE_PINS.items():
        path = worker / name
        s = path.lstat()
        need(stat.S_ISREG(s.st_mode) and s.st_nlink == 1,
             "sidecar source is not a regular file: " + field)
        need(policy.get(field) == sha_file(path), "sidecar source pin differs: " + field)


def self_test():
    import tempfile
    import io
    docker = render_dockerfile("# syntax=docker/dockerfile:1\nFROM x AS managed\n", "a" * 40)
    need("FROM managed AS managed_c1" in docker and "from=managed,source=/" in docker, "same-build stage missing")
    compile(VERIFY_C1.replace("__C1_PINS__", repr(C1_PINS)).replace("__MARKER__", MARKER), "verify", "exec")
    image = "registry.fly.io/example:one"
    product = synthetic_product()
    log = f'#12 0.1 {MARKER}{json.dumps(product)}\n#13 pushing manifest for {image}@sha256:{"b" * 64} done\n'
    with tempfile.TemporaryDirectory() as directory:
        pinned_source = Path(directory) / "pinned-source"
        worker = pinned_source / "cloud/worker"
        (worker / "managed-runtime").mkdir(parents=True)
        policy = {}
        for field, name in SIDECAR_SOURCE_PINS.items():
            path = worker / name
            path.write_bytes((field + "\n").encode())
            policy[field] = sha_file(path)
        policy_path = worker / "managed-runtime/policy.json"
        policy_path.write_bytes(json_bytes(policy))
        validate_sidecar_sources(pinned_source)
        for field, name in SIDECAR_SOURCE_PINS.items():
            path = worker / name
            original = path.read_bytes()
            path.write_bytes(original + b"changed\n")
            try:
                validate_sidecar_sources(pinned_source)
            except ValueError as error:
                need(str(error) == "sidecar source pin differs: " + field,
                     "source mismatch did not identify the changed pin")
            else:
                raise AssertionError("changed sidecar source accepted")
            path.write_bytes(original)
        private_source = Path(directory) / "private-source"
        private_source.mkdir(mode=0o700)
        for root in (private_source / "dist", private_source / "cloud/dist"):
            root.mkdir(parents=True, mode=0o700)
            (root / "module.js").write_bytes(b"export {};\n")
            (root / "module.js").chmod(0o600)
            (root / "main.js").write_bytes(b"#!/usr/bin/env node\n")
            (root / "main.js").chmod(0o700)
        prepare_generated_modes(private_source)
        need(stat.S_IMODE(private_source.stat().st_mode) == 0o700, "private capture mode changed")
        for root in (private_source / "dist", private_source / "cloud/dist"):
            need(stat.S_IMODE(root.stat().st_mode) == 0o755
                 and stat.S_IMODE((root / "module.js").stat().st_mode) == 0o644
                 and stat.S_IMODE((root / "main.js").stat().st_mode) == 0o755,
                 "generated output remains unreadable to the worker")
        archive = Path(directory) / "source.tar"
        destination = Path(directory) / "source"
        destination.mkdir()
        with tarfile.open(archive, "w") as tar:
            item = tarfile.TarInfo("nested/source.txt"); item.size = 5; item.mode = 0o664
            tar.addfile(item, io.BytesIO(b"hello"))
            item = tarfile.TarInfo("nested/executable.sh"); item.size = 4; item.mode = 0o775
            tar.addfile(item, io.BytesIO(b"true"))
        extract_archive(archive, destination)
        need((destination / "nested/source.txt").read_bytes() == b"hello", "archive extraction failed")
        need(stat.S_IMODE((destination / "nested/source.txt").stat().st_mode) == 0o644
             and stat.S_IMODE((destination / "nested/executable.sh").stat().st_mode) == 0o755
             and stat.S_IMODE((destination / "nested").stat().st_mode) == 0o755,
             "archived public source remains mutable or unreadable")
        with tarfile.open(archive, "w") as tar:
            item = tarfile.TarInfo("escape"); item.type = tarfile.SYMTYPE; item.linkname = "/tmp"
            tar.addfile(item)
        try:
            extract_archive(archive, destination)
        except ValueError:
            pass
        else:
            raise AssertionError("archive symlink accepted")
        p = Path(directory) / "log"
        p.write_text(log)
        need(parse_build_log(p, image)[0] == "sha256:" + "b" * 64, "push parse failed")
        for bad in (log.replace(" done", ""), log + log, log.replace(image, image + "-foreign")):
            p.write_text(bad)
            try:
                parse_build_log(p, image)
            except ValueError:
                continue
            raise AssertionError("invalid build proof accepted")
        marker = f'#12 0.1 {MARKER}{json.dumps(product)}\n'
        traditional = f'one: digest: sha256:{"b" * 64} size: 13490\n--> Pushing image done\nimage: {image}\nimage size: 5.5 GB\n'
        need(parse_build_lines((marker + traditional).encode().splitlines(True), image)[0] == "sha256:" + "b" * 64,
             "traditional push parse failed")
        need(parse_build_lines((log + f'image: {image}\n').encode().splitlines(True), image)[0] == "sha256:" + "b" * 64,
             "Depot image summary rejected")
        for bad in (
                traditional.replace("one: digest:", "other: digest:"),
                traditional.replace(image, image + "-foreign"),
                traditional.replace("b" * 64, "b" * 63),
                traditional.replace("size: 13490", "size: 0"),
                traditional.replace("--> Pushing image done\n", ""),
                traditional.replace(f'image: {image}\n', ""),
                traditional.replace("one: digest:", "one: digest:" + " " * 2),
                traditional.replace("--> Pushing image done", "--> Pushing image done unknown"),
                traditional + traditional,
                traditional + f'image: {image}\n',
                f'image: {image}\n' + traditional.replace(f'image: {image}\n', ""),
                "--> Pushing image done\n" + traditional.replace("--> Pushing image done\n", ""),
                traditional + f'#13 pushing manifest for {image}@sha256:{"b" * 64} done\n'):
            try:
                parse_build_lines((marker + bad).encode().splitlines(True), image)
            except ValueError:
                continue
            raise AssertionError("invalid traditional build proof accepted")
        malformed = dict(product, managedFiles={})
        p.write_text(log.replace(json.dumps(product), json.dumps(malformed)))
        try:
            parse_build_log(p, image)
        except ValueError:
            pass
        else:
            raise AssertionError("missing actual managed products accepted")
    print("self-check passed (no compiler, provider or image build)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revision", help="committed Git revision, resolved once")
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--output", type=Path)
    parser.add_argument("--app", default="rudder-workers-exla")
    parser.add_argument("--image-label")
    parser.add_argument("--fly-executable", default="flyctl")
    parser.add_argument("--builder", choices=("depot", "fly"), default="depot",
                        help="remote build service; fly uses the traditional Fly builder")
    parser.add_argument("--prepare-only", action="store_true", help="capture and compile JS only; no provider call")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    need(args.revision and args.output, "--revision and --output are required")
    need(re.fullmatch(r"[a-z0-9][a-z0-9-]{0,62}", args.app), "invalid app")
    # Read-only resolution precedes any output. Never interpolate a shell command.
    revision = subprocess.check_output(["git", "-C", str(args.repo), "rev-parse", "--verify", "--end-of-options", args.revision + "^{commit}"], text=True).strip()
    need(re.fullmatch(r"[a-f0-9]{40}", revision), "invalid revision")
    label = args.image_label or "candidate-" + revision[:12]
    need(re.fullmatch(r"[a-zA-Z0-9_][a-zA-Z0-9_.-]{0,127}", label), "invalid image label")
    output = args.output.absolute()
    need(not output.exists() and not output.is_symlink(), "output already exists")
    need(output.parent.is_dir(), "output parent missing")
    os.umask(0o077)
    disk_check(output.parent)
    intent = output.with_name(output.name + ".intent.json")
    write_new(intent, json_bytes({"sourceRevision": revision, "output": str(output), "buildOnly": True,
                                  "compilerC1Image": C1_IMAGE, "image": f"registry.fly.io/{args.app}:{label}",
                                  "buildService": args.builder}))
    build_id = sha_file(intent)
    output.mkdir(mode=0o700)
    sync_dir(output.parent)
    source = output / "source"
    source.mkdir(mode=0o700)
    env = dict(os.environ)
    env.pop("FLY_ACCESS_TOKEN", None)
    env["RUDDER_SKIP_POSTINSTALL"] = "1"
    commands = []
    def run(name, command, cwd=source, stdout=None):
        row = OriginalCommand(output, name, command, cwd, env, stdout).run()
        commands.append(row)
    archive = output / "source.tar"
    run("archive", ["git", "-C", str(args.repo.absolute()), "archive", "--format=tar", revision], args.repo.absolute(), archive)
    extract_archive(archive, source)
    validate_sidecar_sources(source)
    run("npm-root", ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"])
    run("typescript-cli", [str(source / "node_modules/.bin/tsc"), "-p", "tsconfig.json"])
    run("board", ["npm", "run", "build-board"])
    run("chmod-cli", ["npm", "run", "chmod-cli"])
    run("npm-cloud", ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"], source / "cloud")
    run("typescript-cloud", ["npm", "run", "build"], source / "cloud")
    prepare_generated_modes(source)
    validate_worker_payload(source)
    js, assets = generated_files(source)
    dockerfile = source / "candidate.Dockerfile"
    write_new(dockerfile, render_dockerfile((source / "cloud/worker/Dockerfile").read_text(), revision).encode())
    # Separate worker app/config and target selection prevent deployment.
    config = source / "candidate.fly.toml"
    write_new(config, f'app = "{args.app}"\n'.encode())
    policy = json.loads((source / "cloud/worker/managed-runtime/policy.json").read_bytes())
    node_image = policy["node_base_image"]
    need(type(node_image) is str and re.fullmatch(r"node:[a-zA-Z0-9_.-]+@sha256:[a-f0-9]{64}", node_image), "pinned policy Node image required")
    receipt = {"formatVersion": 1, "sourceRevision": revision, "sourceArchiveSha256": sha_file(archive),
               "sourceArchive": str(archive), "sourceDirectory": str(source),
               "generatedJs": js, "generatedJsSha256": hashlib.sha256(json_bytes(js)).hexdigest(),
               "generatedAssets": assets, "dockerfileSha256": sha_file(dockerfile),
               "compilerC1Image": C1_IMAGE, "compilerC1Pins": C1_PINS, "commands": commands,
               "buildId": build_id, "builderSourceSha256": sha_file(Path(__file__)),
               "buildService": args.builder,
               "workerNodeImage": node_image,
               "image": None, "digest": None, "productData": None,
               "deployed": False, "sdkQualified": False, "runtimeQualified": False,
               "sourceQualified": False, "diskFloorCheckedAtStageBoundariesOnly": FLOOR}
    write_new(output / "prepared-source.json", json_bytes(receipt))
    if not args.prepare_only:
        fly = shutil.which(args.fly_executable)
        need(fly is not None, "Fly executable unavailable")
        fly = str(Path(fly).resolve())
        image = f"registry.fly.io/{args.app}:{label}"
        receipt["flyExecutable"] = {"path": fly, "sha256": sha_file(fly)}
        run("remote-build", [fly, "deploy", ".", "--config", str(config), "--app", args.app,
                             "--dockerfile", str(dockerfile), "--build-target", "managed_c1",
                             "--build-arg", "WORKER_NODE_IMAGE=" + node_image,
                             "--build-arg", "RUDDER_CANDIDATE_BUILD_ID=" + build_id,
                             "--build-arg", "RUDDER_IMAGE_PRESEED_PROFILE=baked-cas-v1",
                             "--build-arg", "RUDDER_IMAGE_QUALIFICATION_NONCE=" + revision,
                             *(["--depot=false"] if args.builder == "fly" else []),
                             "--build-only", "--push", "--remote-only", "--image-label", label])
        digest, product = parse_build_log(output / "remote-build.log", image)
        receipt.update(image=image, digest=digest, productData=product)
    write_new(output / "build-receipt.json", json_bytes(receipt))
    print(json.dumps({"buildOnly": True, "sourceRevision": revision, "image": receipt["image"],
                      "digest": receipt["digest"], "receipt": str(output / "build-receipt.json")}))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Command output is retained privately, never echoed (npm/provider may
        # include account diagnostics). No automatic retry after any stage.
        print("candidate build failed: " + str(error), file=sys.stderr)
        sys.exit(1)
