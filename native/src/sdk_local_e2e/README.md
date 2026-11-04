# Local SDK execution gate

Run `scripts/test-sdk-helper-local.sh` as actual Linux UID/GID1500. It uses only
the queued `/workspace/rudder-build/rcargo` and the `rudder-native` library's
`helper::local_tests` filter. It preserves the wrapper's shared compiler profile.
`--preflight-only` checks inputs and namespace availability without building.
`--real` runs only the real selected SDK path. `--stand-in` runs only the two
older static C carrier probes. Missing prerequisites fail with exit 2 and an
actionable message; an ignored/default-skipped test is not execution evidence.

The real fixture copies Python 3.11.2, Node 22.23.4 and TypeScript 5.9.3 from
installed offline inputs. `pins.json` fixes the principal executable bytes for
the current Linux x86_64 development image. A different image must provide
those exact inputs, or intentionally review/update these local fixture pins.
This does not change the production compiler profile. TypeScript is found in
an ancestor's node_modules, or in `SDK_HELPER_LOCAL_TYPESCRIPT`. Python stdlib,
TypeScript libraries and explicit ELF dependencies are copied to disposable
`/workspace/rudder-sdk-real-*` storage. Every copied file's size and SHA256 is
recorded and reverified before each scenario. `closure.sha256` pins the whole
manifest, including stdlib, TypeScript declarations and dynamic libraries.
No network, package installer,
credential directory, host root mount, or production SDK descriptor is used.

The existing private `run_namespace_retaining` API executes the unmodified
production Python observer, confinement code, selected arguments and compiler.
The source and output directories are captured as descriptors, renamed, then
replaced at their original paths before execution. A real TypeScript compile
must produce the expected JavaScript in the retained output, while both source
trees stay unchanged and the replacement output remains empty. Real compiler
errors and attempts to reference proc or a readable host-only declaration must
fail. Cancellation is tested before start and after an inode access watch
observes the actual compiler reading the retained source. Expiry uses the adopted original 500 ms budget, with a
large real compiler input. No case refreshes the original constraint.

After success or failure the helper requires kernel `ECHILD` for its entire
child table, covering the namespace holder and actual PID1 workload. The outer
supervisor independently waits for that helper. The real observer checks zero
capabilities and readonly mount layout; the explicitly labelled static carrier
probes directly test EROFS writes, mapped identity, NNP and zero bounding caps.
The test watchdog is a failure bound, not a product deadline or settlement ACK.

Each completed real case emits `LOCAL_SDK_REAL_CASE=` JSON and the suite emits
`RUDDER_LOCAL_REAL_SDK_E2E_V1=passed` only after all seven cases. Preserve the
runner's stdout/stderr as evidence when qualifying a platform. The Node entry
point is opt-in:

```
SDK_HELPER_LOCAL_E2E=1 /workspace/rudder-build/rnpm exec -- node --test tests/sdk-helper-local-e2e.test.mjs
```

The test-only UID1500 supervisor replaces the Root parent solely at the private
namespace carrier boundary. It never enters or relaxes production Root/peer,
capsule, catalogue, producer or helper-entry checks, and cannot grant SDK
admission. It does not qualify Buck2 execution, Rust, the full production SDK,
Root custody, cache reuse, Fly, hardware, deployment or release readiness.
The Buck2 file is explicitly inert, since the selected TypeScript path does
not invoke Buck2. Existing stand-in-only tests remain separately labelled.
