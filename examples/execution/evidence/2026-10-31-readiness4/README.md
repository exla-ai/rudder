# Protocol4 kernel readiness qualification

A fresh Fly VM passed actual Spec5/Binding4 descriptor handoff and protocol4
readiness with private USER/NET namespaces, a live backend pidfd, zero values in
all five capability sets and no-new-privileges. The same valid descriptors sent
under protocol3 were rejected. The exact ignored test passed without skips.

The source was a working-tree snapshot, not a deployed product revision. Its
baseline, source envelope, executable digest and derived image are recorded.
The canary used no app secrets, services or mounts. It was destroyed; independent
inventories verified absence. The live home was not restarted.

This qualifies kernel readiness only. It does not qualify source/FUSE launch,
model authority, accepted jj history or default migration. Runtime5 admission
remains separately gated. The first canary correctly failed because trusted
setup lost SETPCAP before clearing the bounding set; adding unshare --keep-caps
until setpriv clears all five sets fixed the fixture without weakening validation.
