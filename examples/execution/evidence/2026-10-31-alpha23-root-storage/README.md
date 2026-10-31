# Alpha.23 root storage capability qualification

The actual stock worker image at revision `ca3eb6ce2c663b6c5069ce2adc094c8b64f8af87`
passed the disposable Fly VM canary. Root and UID1500 both observed `EROFS` after
a bounded tmpfs remount. The unprivileged child retained caps0/NNP1, shared the
same mount namespace and completed the seven packaged executable checks. Normal
unmount, exact owned VM destruction and two independent absence reads passed.

Scope is root storage capability only. No production seed producer, source-model
bootstrap, backend authentication, agent run or preparation-cache qualification
is established by these files. Preparation manifests retain their original zero
provider/mount counts; actual execution is in `physical-*.json`.

The five adapted modules, their five exact baselines, diff, checksums and mock
checks reconstruct the controller/guest bytes used for the physical run. Provider
inventory/env output and credential-bearing logs are excluded. Every actual VM
was disposable; the protected active home was not restarted.
