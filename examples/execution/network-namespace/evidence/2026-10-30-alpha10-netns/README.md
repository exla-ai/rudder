# Alpha.10 UID1500 USER/NEWNET and TUN access observation

One owned Fly Machine, `86510dbe935228`, ran the exact alpha.10 worker image
`sha256:ae1d4bf448c05dc71373efb1b9efd6510dfed4526f76838bc53e1a5c00a446ff`
in `sjc` on kernel `6.12.114-fly`, 2026-10-30. The C probe completed with exit0.
**The original Node evidence wrapper failed a postcheck.** It incorrectly
required the capability bounding set to stay zero after a new user namespace
was created. The preserved observations show the new namespace resets that set.
The separate [offline audit](offline-audit.json) passes the corrected assertions;
it does not turn the original wrapper into a passing execution.

| Measurement | Observed result |
| --- | --- |
| Real managed image bootstrap | UID/GID1500; no supplementary groups; NNP1; all five capability sets initially zero |
| Fresh self-mapped USER | Mapping1500 to1500 succeeds; effective/permitted/bounding sets become `000001ffffffffff` in the new user namespace |
| In-process NEWNET | Succeeds; actual namespace FD has a different inode; `NS_GET_USERNS` matches the newly created user namespace FD |
| In-process loopback setup | `SIOCSIFFLAGS` raises `lo`; actual flag read confirms it |
| External exec as UID1500 | Effective/permitted/inheritable/ambient sets become zero; bounding stays `000001ffffffffff`; loopback mutation fails `EPERM`1 |
| Explicit in-process capability drop | Effective/permitted/inheritable/ambient sets zero; loopback mutation fails `EPERM`1 |
| `/dev/net/tun` | Character10:200, mode0600, owner/group appear as unmapped65534 inside the new user namespace; `open(O_RDWR)` fails `EACCES`13 |
| Private TAP | Not attempted because opening TUN failed; no persistent device was requested or created |
| Probe cleanup | C probe restored original guest loopback state, closed its witness/control FDs and reaped its comparison child |
| Canary cleanup | Exact ID/name/nonce absent in both controller inventories and a third, separate-process [provider inventory](independent-cleanup.json) |

The device node's existence does not establish working TUN driver access or
`TUNSETIFF` permission for this worker. No device ownership/mode changes, device
creation, host interface changes, broad root grant, packet helper or privileged
broker was attempted. Capabilities obtained in a new user namespace do not grant
authority over the parent namespace. The
[Linux user namespace documentation](https://man7.org/linux/man-pages/man7/user_namespaces.7.html)
describes namespace-scoped capabilities and exec recalculation; the
[kernel TUN documentation](https://docs.kernel.org/networking/tuntap.html)
describes the device access and network-administration requirements.

The wrapper assertion happened after all C measurements and before its
post-probe parent-namespace/status and generated-file removal checks. Those
postchecks were not measured and are not claimed. The parent-before witness and
C-before witness agree; the C program unshares only itself. Canary deletion
removed its remaining runtime files. Raw proof/report bytes are preserved.

The immutable-digest create succeeded on this occasion. The provider resolved
the requested digest exactly; no tag fallback or second Machine was used. The
preload begins automatically during boot, so controller-side digest verification
and the probe are not an explicit admission barrier. A future tag-based driver
must wait for a host-written gate after verifying the resolved digest before
starting its namespace measurements.

The configuration had no services, mounts or account/provider/workspace
credentials. App secrets were verified empty before creation. The real bootstrap
and cache entrypoint ran, with agent updates disabled; a Node preload held before
the home/supervisor module evaluated. Credential-shaped environment fields were
checked absent without recording names or values. Only the archived small C
probe was compiled, using image cc12.2.0; no product build ran. Its sanitized
environment contains only PATH, HOME, TMPDIR, LANG and LC_ALL.

No host listener scan, `connect`, `bind`, `listen`, `setns`, external network
target, provider login/model call, user profile, auth/session file, active home
or control-plane exec was used. AF_INET datagram sockets were opened solely for
ioctls on `lo` after verifying the new network namespace. This is one
image/kernel/UID observation, not general Fly support, connectivity, egress,
guest preview or managed/default activation qualification.

## Evidence and exact sources

- [Controller report](report.json), [unaltered guest proof](guest-proof.json),
  [original cleanup](cleanup.json), [independent cleanup](independent-cleanup.json).
- [Frozen source manifest](source-manifest.json), [source-only archive](source.tar.gz),
  [machine config](machine-config.json), [ownership state](state.json).
- Actual controller, preload and C source are archived before provisioning.
  Four allowlisted alpha.10 boot/Docker source files were checked byte-for-byte
  against revision `56f825ea3c1d6136b1a20b3031f835ef33f870cd` before archiving.
  No recursive credential/runtime source collection was used.
- The later [offline audit](../../audit-evidence.mjs) and
  [independent absence helper](../../verify-cleanup.mjs) are post-run evidence
  tools, outside the pre-provisioning source archive. Their hashes are included
  in [SHA256SUMS](SHA256SUMS).

The next narrow candidate remains an explicitly named **qualification-only**
NEWNET/public-HTTPS CONNECT profile, with setup in-process before exec. General
Cloud networking still needs a qualified packet path for raw Node/native TCP,
Git SSH and arbitrary build tools. A slirp4netns/libslirp or pasta candidate must
independently enforce host/public-address, private/link-local/metadata, Fly6PN,
DNS-rebinding and IPv6/translated-address policy. NEWNET also means host
`/proc/net` scanning cannot discover guest previews; owned guest listener
registration and a narrow private preview relay remain separate gates. See the
[network design](../../../../../docs/architecture/managed-network-isolation.md).
