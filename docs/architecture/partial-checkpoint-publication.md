# Partial checkpoint DATA publication

This path replicates retained source-complement bytes and audit documents. It
does not establish current execution authority, physical writer drain, accepted
jj history, a new accepted source/seed pair or a complete local root pin. A
historical `QuiescedPartialCheckpointV1` (Q) can supply DATA without reviving the
execution that originally produced it. The executor's live drain and frozen-view
join remains a separate protocol.

## Graph and identity

`partial_checkpoint::prepare_partial_checkpoint_data` takes an explicit live
caller-owned staging lease, Q and `PartialCheckpointTemplateV1`. The template
contains the exact `RemoteLaunchRequest`, base SourceV2/RunnableRoot descriptors
and workspace profile. It supports only the empty source-only profile,
environment `None` and canonical empty hardlink topology. The descriptor hashes
must equal the selected base, accepted source, profile and projection policy.
The accepted private seed is an audit identity and is never read or hydrated.

The new SourceV2 and RunnableRoot1 share the capture plan's Directory. They
preserve the selected policy/profile/topology and the base knowledge cursor;
they invent no knowledge acknowledgment. SourceV2 is a valid quiesced
source-complement DATA descriptor. It is not a claim that ignored/untracked
files, mode normalization or conflict handling match a new jj versioned tree.
Creating a new accepted source/seed pair still requires the actual private jj
provider and separately authorized acceptance transaction.

The publication is an ordinary ActionResult `ReferencedMetadata` envelope with
exact fields `format_version`, `references`, `payload`. Payload1 has exactly:

```
format_version kind source_role publication_id template
q source runnable plan freeze_intent drain_record
```

`kind` is `partial-checkpoint-data-v1`; `source_role` is
`quiesced_source_complement_data`. The immutable publication ID is `pcd1-`
followed by raw SHA256 hex of the canonical payload with only `publication_id`
omitted. The ActionResult ref hashes the complete envelope. No self-hash cycle
exists.

The six sorted unique strong refs are Q Blob, SourceRoot, RunnableRoot, plan
Blob, freeze Blob and drain Blob. Sort uses Rust `ObjectRef::Ord`: kind ordinal
(`blob,file,directory,directory_shard,source_root,environment_root,checkpoint,
runnable_root,action_key,action_result`), hash algorithm, digest and u64 size.
Template/base/seed references are prior audit identities rather than extra
strong edges. The new runnable graph reaches its source directory, profile,
policy and topology in the normal way; immutable inheritance is not copied.

## Producer retention and validation

Preparation renews the dedicated staging lease before reading producer CAS
documents. It validates the original capture and exact two-Blob evidence ACKs,
Q/base/attempt/observation/stop/cursor/prior-head bindings, canonical freeze and
drain documents, and normalized explicit delta provenance. The drain document
is historical audit DATA, including u64 namespace identities, not a reconstructible
process or namespace guard. A typed historical read never creates a live grant.

Preparation returns bounded bytes without writing new wrapper objects. The
caller must durably record `PartialCheckpointDataIntent` (version, original
staging UUID, immutable publication ID/ref) and the original Q plus template
before `retain` or network effects. Existing durable execution/template records
can supply those exact inputs; otherwise the caller journal must contain them.
Intent alone cannot reconstruct missing Q/template bytes. `retain` writes and ACKs
exactly four new units: Q Blob, SourceV2, RunnableRoot1 and ActionResult. The pin
name is `partial-checkpoint:producer:<publication_id>`. Capture-local data plus
plan and the two evidence Blobs keep their original separate producer receipts.
Errors do not roll back or release uncertain producer effects.
An existing producer ACK directs `retain` to explicit current-DATA
reconciliation rather than silently treating an observation as a fresh ACK.
Separate read staging remains held while the original write stage is consumed
and through all post-ACK validation reads.

`reconcile_partial_checkpoint_data` acquires separate recovery-owned staging
before reads, rebuilds the expected immutable bytes from the durable inputs,
and checks the original operation's staging UUID and exact set/publication.
It never revives an expired original lease, rewrites wrappers or releases
producer retention. Its non-Serde `CurrentPartialCheckpointDataRetention` is an
observation of CURRENT same-operation DATA, with an opaque transfer receipt;
it is not the recovered first ACK or an original producer cleanup capability.
The recovery lease is removed only after synchronous reads actually settle.

A pre-ACK intent cannot know the ledger generation allocated later. If a
conservative crash-stage was explicitly released/reused with the same UUID,
unknown-ACK recovery cannot distinguish the newer same-operation generation.
It reports current DATA only. A known persisted receipt still compares its
entire generation and rejects that replacement. A subsequent transfer always
allocates and ACKs its own independent clone before enabling source reads.

`PartialCheckpointDataReceipt` carries exactly `format_version`,
`publication_id`, `publication` and the original `wrappers` ObjectSetReceipt.
Generations and other counters stay u64. JavaScript must preserve this receipt
as opaque JSON rather than parse and stringify it through `number`.

Rust validates the historical retained documents and exact local units without
walking inherited content. Remote publication must independently verify every
boundary path against the exact account-scoped immutable base and validate the
complete resulting graph, semantic child relationships and budgets. Local CAS
presence is not proof of inherited provenance or accepted history.

## Transfer ownership

`TransferSession::pin_partial_checkpoint` starts its own staging lease before
any producer receipt or CAS read. It validates all three original producer
groups, computes the exact sorted union of capture-local units including plan,
the two evidence Blobs and the four wrapper units, and reserves its expected
staging/set identity before committing a transfer-owned ACK. Only the exact
successful clone enables binary source reads. Original producer pins may
legitimately retire after validation; the clone then owns the bytes independently.

The new native RPC operation is:

```
{"operation":"pin_partial_checkpoint","receipt_json":"<opaque JSON>"}
```

It returns exactly twelve inventory fields:

```
format_version owned_pin publication_id publication q source runnable
plan freeze_intent drain_record base objects
```

`owned_pin` starts with `transfer:partial-checkpoint:rpc:` and has a fresh UUID.
The inventory exposes refs and bytes, not JS numeric receipt counters or a
complete-root `.receipt`. It cannot enter `inventory`/complete closure consumers.
`unpin_source`, EOF and session Drop retire only the session's resources.

Known ACK cleanup compares the entire receipt including generation. If the ACK
generation is unknown, cleanup never queries/releases a current pin by name:
the same original staging UUID and object set can have been reused after a
conservative crash-stage. It reports unconfirmed cleanup and preserves the
pending entry plus any still-existing owned GC stage for explicit recovery.
Consumed stages are never revived. A process kill can leave conservative pins;
this slice adds no first-ACK archive or automatic orphan retirement.

## Bounds and publication recovery

Q is at most32KiB, freeze64KiB, drain32KiB, plan4MiB, and canonical template and
envelope64KiB each. Base profile/policy refs allow at most1MiB; oversized embedded
templates still fail the separate64KiB envelope limit. Local delta units are
sorted unique content refs at most16000; remote boundaries are sorted unique,
disjoint from local refs, at most4096, with at most1024 edges each. Derived
directory and empty topology each need explicit provenance.

The transfer union is at most16384 objects and4GiB. A Blob is at most256MiB,
other metadata4MiB, and File/DirectoryShard/RunnableRoot descriptors64KiB.
Session source/partial pins and staging leases each have a32-entry admission
bound. Validation has a cooperative30s budget and staging lasts600s. Store lock,
filesystem and SQLite settlement still follow the store's blocking local APIs;
caller cancellation is not permission to free resources before work settles.

An uploader checks configured provider/tenant and exact immutable base, clones
first, and uses this ActionResult as the immutable publication target. A lost
reply reconciles GET for that same ID, scope and envelope ref. It must not
remint identity, recapture, retry an uncertain POST blindly or remove producer
retention. Reader settlement precedes transfer unpin. Cloud staging cleanup is
account-scoped and does not delete objects or published roots.

The existing partial-tree format and complete-root APIs keep their original
bytes and meanings. This DATA path does not activate a managed profile or
permit a saved Q/bundle receipt to authorize a launch or acceptance transaction.

## Actual owned source5 publication

The [Fly qualification](../../examples/execution/evidence/2026-10-31-source5-publication/README.md)
composes actual source5 shutdown with this DATA path. It persists original inputs,
reconciles a lost producer ACK, transfers the actual managed CAS and reconciles a
lost successful publication response with GETs and zero further uploads. After
producer destruction, a separate readback continuation verified the exact receipt,
14-object closure, original Q/evidence and both changed and inherited file bytes.
The first independent read failed with an uninformative diagnostic and remains
recorded as unqualified. The successful continuation preserves that history and
the original capture consistency. It does not enable default launch or history
acceptance.
