# Garment tracking

Closet can pair passive UHF RAIN tags with the inventory already in the app.
The reader supplies observations; Rust decides the status. No model, phone NFC,
tag-memory programming, or daily check-in is involved. Tracking starts disabled.

## One status per physical garment

| Confirmed evidence | Status | Ready to wear |
| --- | --- | --- |
| Closet | Clean / ready | Yes |
| Rewear rack | Worn / ready to rewear | Yes |
| Laundry hamper | Needs laundry | No |
| Dry-cleaning bag or fixed holder | Queued for dry cleaning | No |
| Absent after hamper | In the wash | No |
| Absent after dry-cleaning bag | At the dry cleaners | No |
| Absent after closet or rewear rack | Being worn | No |

These are household assumptions, not proof that a machine ran or a cleaner
finished. Continued absence does not change washing or cleaners status. A new
confirmed area replaces the old context: cleaners → closet → absent means
being worn. Returning to the closet confirms clean/ready after the ordinary
presence threshold. There is no return dock or cleaning-confirmation screen.

`last_confirmed_area` is internal evidence, distinct from the current area. It
survives absence. Unpaired garments, no-tag exceptions, and new bindings without
a baseline never become worn merely because they have not been read.

The inventory remains complete. RFID does not archive items, change ownership,
or change quantity. **Ready to wear** is a separate filter. A catalog card is
ready when at least one active physical copy has a ready baseline; its details
show each copy separately. Combined positive readiness, status and area filters
must match the same physical copy. `ready_to_wear=false` means no active copy is
ready anywhere, not merely that a different copy happens to be in the hamper.

Today excludes items whose active copies are all known unavailable, before any
optional generation. Untracked or explicitly exempt copies keep catalog-only
behavior, without claiming RFID certainty. A ready last-known state remains
ready during an outage, with health shown separately. Results are rechecked
when published and become stale if a referenced item becomes unavailable.

## Where the data lives

| Data | Authority |
| --- | --- |
| Names, brands, sizes, colors, quantities, images, stable catalog IDs | Existing TOML catalog, or compatible JSON input |
| Physical unit UUIDs and never-reused ordinals | SQLite |
| Active and retired bindings, installation details, no-tag exceptions | SQLite |
| Logical areas, readers, ports, thresholds, commissioning flags | Versioned SQLite tracking configuration |
| Current status, last area, manual baselines, transition audit | SQLite |
| Durable reader ownership and configuration-restoration intents | SQLite |
| Credentials | Private process environment or owner-only secret file |
| Fresh coverage, raw sightings, candidate captures and diagnostics | Bounded runtime evidence; deliberately re-earned after restart |
| HA entities | Projection of Rust state |

Catalog sync reconciles tracking in the same SQLite transaction. Metadata edits
do not replace physical units. Missing catalog IDs are archived with their
bindings retained; restoring the same ID restores that identity. Catalog and
tracking revisions are separate. Radio observations never rewrite catalog
files, increment the catalog revision, or require a Git commit. Only tracking-
filtered pagination depends on the tracking revision.

A known quantity allocates that many stable units. Increases add new UUIDs and
ordinals. Decreases require explicit retirement/reconciliation; the app does not
guess which physical copy was sold. Retired ordinals are never reused. A legacy
item without a quantity cannot enroll until an operator explicitly declares its
physical count, or fixes the authoritative catalog. No automatic quantity of one.
New-unit allocation is bounded at 20,000 active
physical identities; this never rejects or changes the catalog. Capacity-limited
records expose their unallocated quantities and cannot appear fully tracked.
Existing/restored identities are retained even when capacity is exhausted.

For a multi-piece purchase whose components move separately, represent the
components as separate stable catalog records before enrollment, each with its
own quantity and tag. A single tag tracks one independently movable component,
not a whole set. Keep the shared purchase/fitting metadata on those records.

## First garment

1. Sync the existing catalog normally. The same photos, filters and search are
   used for enrollment; no garment description needs retyping.
2. Configure the reader and antenna mappings in **Tracking settings**. Use a
   dedicated enrollment position, or explicitly borrow a monitored antenna.
3. Open **Tags**. Select the physical copy from the untagged queue using
   its photo, brand, category, source and search.
4. Put only that garment at the reading position. Wait for one fresh isolated
   candidate, then confirm. The existing factory EPC is associated with the unit.
5. Remove the garment and continue. Each confirmation or skip starts a new
   capture. The queue position is durable and can be resumed later.
6. Put it in its normal area. Confirmed ordinary inventory establishes its first
   area baseline. An optional explicit baseline is a manual assertion, not a
   claim that the enrollment scan located the garment.

Multiple tags block confirmation. Already-bound ambient tags remain visible,
including the selected garment's old tag during replacement. Separate those
tags instead of relying on signal strength. A changed selection, changed
candidate set, stale capture or disconnected reader invalidates confirmation.
The server rechecks revision, capture version, selected unit and uniqueness in
one transaction. Exact retries return their original receipt.

The UI supports keyboard controls, large photos, skip/resume and progress.
Feedback is visual by default; sound is optional. **Scan to find** looks up an
existing binding without changing it or moving a garment.

### Installation and maintenance

Use comfort samples before a whole-closet installation. An existing sewn-in care
label is the preferred attachment position when suitable; the installer decides
what works for the particular garment. Its printed barcode or retail RFID label
is not automatically a compatible, unique, laundry-safe tag.

The installation queue reuses inventory photos. Optional model, placement and
short notes can be recorded, or a garment can be marked **No tag** with a reason.
No-tag garments stay usable in the inventory and do not infer absence.

**Replace tag** preserves the unit UUID, confirmed status and history, retires
the old binding,
and requires a fresh capture for the new EPC. Old-tag reads cannot move the unit.
**Unbind** removes a mistaken binding without deleting its history. An audited
**Undo** is allowed only when subsequent changes and uniqueness checks permit
it. A replacement's new tag cannot silently take over another unit's identity.

**Set area baseline** establishes a manual current baseline. **Correct this state** records
an asserted status and clears location context rather than inventing an area.
Both fence out earlier observations. Fresh confirmed positives resume automation;
a status-only correction cannot infer absence until a new area is established.
**Pause automation** is the separate explicit persistent pause. Old buffered data
cannot immediately undo a correction.

## Evidence and health

Defaults are commissioning starting points, not RF performance promises:

| Setting | Initial value |
| --- | --- |
| Presence confirmation | 3 reads spanning 1.5 seconds |
| Additional movement hysteresis | 2.5 seconds |
| Absence confirmation | 120 seconds of healthy required coverage |
| Recovery warm-up | 30 seconds |
| Maximum coverage gap | 5 seconds |
| Enrollment freshness | 5 seconds |
| Maximum reader-event age | 60 seconds |

Areas can override their presence, movement and absence thresholds. Multiple
readers/antennas can cover one area. RSSI is diagnostic evidence, not distance
and not a strongest-sample area selector. Conflicting area sightings retain the
previous status and expose ambiguity. One missed or stray read cannot move an
item. Antenna power/dwell changes require discovered reader capabilities and
supported readback, not guessed limits.

Every enabled area needs explicitly configured, enabled, commissioned required
monitor coverage. All required ports must have fresh verified inventory and
finish warm-up before absence can advance. A potentially unreadable enabled
area suspends absence inference. Valid stable positives can still update state.

A network connection or heartbeat does not prove an empty area was inventoried.
Faults, interrupted scans, stalled inventory, borrowing, reconnects, restart and
configuration changes reset coverage/absence evidence. Downtime is not added to
absence timers. The displayed state is retained. A dry-cleaning holder must
remain powered and scanning when the bag leaves; a reader leaving with the bag
is an outage, not proof of a trip to the cleaners.

### Reader adapters

The normalized input includes EPC, reader/port, observed and received time,
session/sequence, purpose, read count, optional TID/RSSI/event reference, scan
coverage and capability readback. EPCs are uppercase even-length hexadecimal,
2–128 characters, preserving leading zeros; they are not restricted to 96 bits.
TID is optional. Unknown EPCs never create catalog entries.

Duplicate sequences/events, old reader sessions, delayed or out-of-order reports,
invalid identifiers, unconfigured ports and oversized batches cannot become
fresh presence. Reconnects establish a new reader epoch. Input and diagnostics
are bounded; readers aggregate reports rather than writing a state per RF read.
An audit event is persisted for a meaningful status/binding/correction change,
including before/after, time, reason, evidence references and configuration version.

The R700 HTTPS IoT interface supplies positive sightings and faults. Its
documented v1 stream does **not** prove completed empty scans; antenna activation
is not scan completion. IoT-only areas cannot enable automatic absence.
Its actual stream configuration
must confirm `eventBufferSize: 0`; buffered or unknown configuration is rejected.
Set this in the reader WebUI, not by granting Closet broader write access.

The managed local LLRP adapter uses completed per-antenna AISpec inventory for
coverage. It claims only a configured ROSpec ID, verifies its fingerprint, and
persists intent before changing owned reader configuration. It never removes an
unrecognized ROSpec or any AccessSpec. Borrowing and shutdown restore guarded
settings; interrupted or externally changed configuration requires recovery.
LLRP is unencrypted TCP on the R700, not authenticated TLS. Use an isolated
private reader network. See [hardware and wire requirements](rfid-hardware.md)
for the exact documented firmware/protocol basis and unverified hardware gaps.

Enrollment and test reads never drive normal area inference. Borrowing pauses
affected coverage until the adapter acknowledges setup, restores it afterward,
and requires ordinary warm-up. A failed restore remains visible rather than
quietly restarting absence inference.

The HA app allows 120 seconds for graceful shutdown, including accepted work,
bounded reader restoration and closing SQLite. This exceeds Supervisor's
[default 10-second stop timeout](https://contributors.home-assistant.io/apps/reference/configuration/).

## Configuration and CLI

Start from the [fictional configuration](../config/tracking.example.json).
The layout is illustrative, not a four-antenna coverage recommendation. Leave
tracking disabled until the real ports, region, power and coverage are verified.

```sh
ha-closet tracking config validate --input tracking.json
ha-closet tracking status
ha-closet tracking config apply --input tracking.json \
  --if-match '"0"' --idempotency-key configure-first-reader
ha-closet tracking history --limit 50
ha-closet tracking export --output tracking-backup.json
```

Mutations require the current **tracking** ETag and a stable idempotency key.
Do not substitute the catalog ETag. Read the resulting revision before the next
new operation. The CLI also supports enrollment, lookup, correction, baseline,
replacement, unbind, retirement, undo, raw normalized batches and typed command
files. See [CLI reference](cli.md) and [OpenAPI](../openapi/api.json).

For Basic authentication, configuration stores reference names, never secret
values. Environment resolution is limited to dedicated `CLOSET_READER_*` names,
not unrelated Home Assistant or provider credentials. Alternatively use
`--reader-secrets-file` / `CLOSET_READER_SECRETS_FILE`, an owner-only regular JSON
file mapping those names to values. A supplied file never falls back to broader
environment credentials. Use `--reader-ca-file` / `CLOSET_READER_CA_FILE` for a
private PEM CA; certificate and hostname verification stay enabled. In an HA
app, `/data/reader-secrets.json` and `/data/reader-ca.crt` are discovered when
present. Restart after changing credential or CA files.

Reader network addresses, credentials, EPC mappings, exports and logs belong in
private storage, never a public fork or build artifact. Routine logs use fixed
error codes without payloads, credentials, EPCs or garment names.

## Home Assistant

The existing integration polls Rust's tracking projection on its shared
coordinator, at most once per minute. Its thin projection uses revision-bound
pages, so restoring a large inventory cannot overflow one HA response. An
interrupted read never publishes a partial set of pieces. Aggregate sensors
expose status counts and
tracking health. Optional per-piece enum sensors have stable unit-based IDs and
are disabled by default. Enable only the pieces useful for automations.
Connectivity controls entity availability; a garment in the wash is an available
sensor with an unavailable-to-wear status, not a disconnected entity.

Reload/removal follows the integration's existing cleanup. Archived/retired
pieces are removed from the active entity projection; restoration preserves the
unit identity. Raw EPCs, RSSI, reader credentials and candidate captures are not
entity attributes or normal HA diagnostics. Rust owns the status table; HA
automations do not need to repeat it.

## Backup and recovery

The existing cold HA app backup includes `/data`: SQLite, credentials and custom
reader trust. Stop the app before a file-level SQLite backup, or use SQLite's
backup mechanism; copying a live database without its WAL is not a backup.
HA entity state is not the tracking authority.

The versioned tracking export includes units, active/retired bindings,
configuration versions, audit history, idempotency receipts and durable reader
ownership/restoration operations. It excludes catalog metadata and secrets.
The CLI writes it atomically with mode 0600 and never overwrites a destination.
For restore, sync the same stable catalog IDs first, then import under the
current tracking revision. Import validates identities and uniqueness and resets
live evidence. It does not rewrite the catalog. Protect and test the complete
SQLite backup for larger histories; bounded JSON exports fail explicitly rather
than silently truncating data.

After any restore, reconnect or process restart, garment states are retained
but radio coverage must be freshly earned. Review recovery-required reader
configuration before allowing automation. Do not delete someone else's reader
configuration to make a connection succeed. Disable a managed reader and wait
for confirmed cleanup before removing it, changing its connection or importing
tracking data. Unresolved claims retain the old connection for recovery, even
when tracking is disabled. Closed session history stays restorable after a
reading position is removed.

## Before hardware arrives

`ha-closet tracking replay --input scenario.json` uses the same reducer, SQLite
operations, catalog reconciliation and enrollment checks in a new in-memory
database. It has no production database path, API destination or reader worker.
Scenario times are explicit and semantic output is deterministic. Live time is
sampled only after serialized SQLite admission, so a waiting request cannot
arrive behind a newer background tick. Explicit replay times remain strict. Simulated
batches/exports cannot enter live tracking state. A simulator configuration is
not a live reader connection.
See [the replay example](../config/tracking-replay.example.json).

Software tests cover HTTP/auth/retries, unit identity, actual SQLite, reader
framing/protocol peers, enrollment in a real browser and HA lifecycle. These do
not establish RF coverage. Commission with actual tagged garments, not empty
air or a single hand-held sample:

1. Densely packed hanging garments, including material/orientation variation.
2. An empty, partly full and full hamper; then remove only some pieces and
   verify the remaining contents stay present.
3. Remove the dry-cleaning bag while its fixed reading position keeps scanning.
4. Adjacent areas and deliberate cross-reads; no stray closet read may clean an
   item still confirmed in the hamper.
5. Representative damp laundry and layered/folded garments.
6. The final concealed antenna placement, including decorative panels/cabling.
7. Disconnect a reader, interrupt inventory and fault a required antenna. Confirm
   states persist and recovery needs fresh warm-up.
8. Check every sample's factory EPC for uniqueness, comfort and care suitability.

Record actual firmware, regional SKU, antenna/cable setup, readback, thresholds
and tested failure cases privately. Mark ports commissioned only after those
tests pass. A full-hamper failure calls for installation/coverage changes, not a
shorter absence timeout or a claim that software proved the clothes were gone.
