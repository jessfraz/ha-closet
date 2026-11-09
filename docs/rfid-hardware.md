# RFID hardware and commissioning

Start with comfort samples and one reading area. Software tests do not establish
that a packed closet, a full hamper, or a particular concealed antenna can be
read reliably.

## Reader modes

| Local interface | What Closet can establish | Connection |
| --- | --- | --- |
| R700 IoT Device Interface | Fresh tag sightings, inventory status, antenna connection changes, stream faults. **Not empty-area coverage.** | Verified HTTPS and optional HTTP Basic credentials supplied by secret reference. |
| R700 LLRP | Completed, duration-controlled single-antenna inventories, with configured participation, antenna connectivity and fault monitoring. | TCP, normally port 5084. R700 LLRP does **not** provide TLS authentication. Use an isolated private reader network. |

Neither mode needs a cloud account, Kafka or an MQTT broker. Only one RFID
control interface can be selected on the reader at a time. The IoT adapter
requires API 1.8 or newer, corresponding to Octane 8.2 or newer. Earlier IoT
versions exist but are not this adapter's compatibility contract. The LLRP
implementation follows LLRP 1.0.1 and the R700 Octane 10.4 manual and manufacturer
LTK 10.58 definitions. Older firmware is not an RF-tested compatibility claim.
[Impinj interface overview](https://support.impinj.com/article/32123172324755),
[release notes](https://support.impinj.com/article/32170876041363),
[current R700 manuals and LTK definitions](https://support.impinj.com/article/360011676720).

The current IoT stream has no boot/session ID, sequence number or scan-completion
event. `antennaActivationEvent` occurs **before** inventory. A CRLF keepalive,
`status: running`, a connected antenna, and even a tag read do not prove that an
empty area was scanned. The old overview's capitalized `/api/v0` events are not
the modern `/api/v1` schema. IoT-only installations retain valid positive
inference but suspend automatic absence.
[Published OpenAPI](https://support.impinj.com/article/32195454977555).

The LLRP adapter owns one explicitly configured ROSpec ID. Each AISpec has one
antenna and a duration, not a tag-count stop condition. It uses Session 2,
Dual Target B-to-A Select, and explicitly disables low-duty cycling and
intelligent antenna skipping inside its own inventory command. A fresh ROSpec
start, the expected per-port AISpec completion, connected-antenna evidence and
no intervening fault are required before emitting verified coverage. That
proves execution, not that every physical tag was readable.
[GS1 LLRP, sections 11 and 14](https://ref.gs1.org/standards/llrp/1.1.0/),
[R700 Octane manual and machine-readable definitions](https://support.impinj.com/article/360011676720).

## Before buying or installing

- Confirm the **country, regional SKU, hardware revision and firmware** with the
  supplier. R700 and R700v2 have different HLA/revision identifiers. Do not choose
  firmware or radio settings from the product-family name alone. The current
  R700 download page distinguishes regional `241`, `341`, `441` and `-B` models;
  the installation manual also covers legacy FCC `141` hardware. Use the actual
  label and manufacturer certification table to select the correct unit.
- The R700 has four **RP-TNC RF ports** and an Ethernet/PoE connection. More
  antennas require supported hubs or additional readers, not passive cable
  splitters. Four logical areas do not imply four antennas will cover them.
- Prefer **IEEE 802.3at PoE+**. The permitted transmit power depends on region,
  hardware and available power. Ordinary 802.3af PoE has restrictions. Check
  actual power negotiation/readback, including LLDP support, before RF testing.
- Use manufacturer-approved antennas and a professionally checked power budget.
  For an A5020's SMA-female connector, specify a **50-ohm RP-TNC-male to
  SMA-male** assembly with known loss at the operating frequency. Observe that
  assembly's bend radius, strain relief and connector instructions. There is no
  universal coax-length limit; cable loss and legal antenna gain matter. Ethernet
  distance limits are not RF-cable specifications.

[R700 quick start](https://support.impinj.com/hc/article_attachments/22893542233107),
[installation manual and certified-region links](https://support.impinj.com/article/360011676720).

## Antennas and tag samples

| Candidate | Practical distinction |
| --- | --- |
| Times-7 A5020CP | Circular polarization, roughly 150 mm square. Current product specification lists 5 dBiC gain. |
| Times-7 A5020MR | Same compact family, lower gain at -3 dBiC for a more confined reading zone. Not a guarantee against cross-reads. |
| HID LinTRAK C10XS-M730 | 10 × 30 mm textile tag. The chip area is up to 2.2 mm thick; the rest is under 1.5 mm. Sample the actual placement against skin. |
| Fujitsu WT-A533G | Thin garment-tag candidate, specified at 0.6 mm. Do not substitute the thicker WT-A533 linen tag or assume their care limits match. |

The A5020 CP/MR use SMA-female connectors. Choose the antenna's FCC 902–928 MHz
or ETSI 865–868 MHz variant for the actual reader region. MR's lower gain is
useful when reducing adjacent-area reads; only testing the final mounting proves
whether it is enough.
[Times-7 CP](https://cms.times-7.com/rain-rfid-antennas/antennas-by-type/far-field-antennas/a5020cp-circularly-polarised-antenna),
[CP versus MR](https://cms.times-7.com/blogs/2023/what-is-the-difference-between-our-a5020-cp-vs-a5020-mr-antenna).

HID documents sewing into a hem or fabric label, or using an oversize heat-seal
patch. Its laundry warranty is conditional on the manufacturer's test conditions.
Fujitsu describes heat-seal labels, sew-on labels and pouches, and its launch
specification excludes high-pressure extractors. Those industrial claims do not
change the garment's care instructions or establish comfort.
[HID datasheet](https://www.hidglobal.com/sites/default/files/documentlibrary/idt-ts-lintrak-c10xs-m730ds-en.pdf),
[Fujitsu WT-A533G manufacturer description](https://info.archives.global.fujitsu/jp/group/frontech/en/about/resources/news/press-releases/2020/1029.html).

Use the existing care label when suitable. Test a few real samples before
attaching hundreds: softness, bending, rubbing, bulk, drape and visibility matter.
Do not stitch through the chip or antenna, fold it sharply,
or heat-seal a delicate garment without the tag maker's attachment procedure
and the garment's permitted care conditions. A soft enclosing label or pouch
may help, but does not guarantee comfort. Keep an explicit no-tag exception for
anything that cannot be made comfortable.

## Commission each area

1. Select the correct region and RFID interface. Configure NTP and inspect
   firmware, certificate, antenna mapping and power readback. Keep credentials
   in the existing private secret file or environment resolver; the database and
   exports contain reference names only. Environment references must use the
   `CLOSET_READER_` namespace; explicit private file-map entries cannot fall back
   to other process credentials. Never use an insecure-TLS workaround.
2. Map every enabled physical antenna to a stable port and logical area. Mark
   required coverage explicitly. Keep the dry-cleaning antennas in the fixed
   holder/cabinet so removing the bag leaves a working reading area behind.
3. Begin at the lowest professionally approved power that consistently reads
   the real samples. The LLRP software starts with 250 ms per-port AISpecs unless
   configured otherwise. This is a tuning starting point, not a coverage promise.
   Set the allowed scan gap to accommodate the full configured scan cycle.
   IoT preset transmit power is readable; IoT does not offer this adapter a
   documented per-antenna dwell setting. A whole-profile duration trigger is not
   a per-antenna dwell.
4. Test densely packed hanging clothes; empty, partly full and full hampers;
   partial hamper removal; the bag leaving its holder; representative damp
   laundry; adjacent areas; and the actual concealed installation, including
   decorative panels. Reposition/add antennas before compensating with power.
5. Unplug an antenna, interrupt inventory, disconnect the network and restart the
   app/reader. States must remain stable during the fault. Fresh coverage and
   warm-up must be earned after recovery. A heartbeat alone must never resume
   absence inference.

Use the enrollment position with one garment at a time, then confirm its factory
EPC against the existing inventory photo. Enrollment does not write, lock,
password-protect or kill tags. Phone NFC is not a UHF reader.

## Recovery and limits

IoT history can contain up to 300,000 buffered events, but there is no replay
cursor or end-of-history marker. Configure HTTP streaming with
`eventBufferSize: 0` in the reader Web UI before using this adapter. Actual
readback must explicitly confirm zero; missing or nonzero buffering suspends
the worker with a recovery diagnostic. Closet does not change that setting.
Reconnect queries reader time, changes the local epoch and rejects old reports.
Accepted clock skew cannot regress normalized time on the next connection.
Slow/stalled streams are closed and retried with bounded backoff. Keepalives
are transport evidence only.
[Impinj FAQ](https://support.impinj.com/article/1500005196002),
[stream schema](https://support.impinj.com/article/32195454977555).

LLRP changes are claimed durably before reader mutations and confirmed by
readback. Unknown ROSpecs, any AccessSpec, or mismatched configuration stop setup.
No blanket reset, delete-all, or tag-memory command exists in the adapter.
Reconnect stops and flushes only the owned program, then requires a fresh start
and timestamp barrier. Shutdown stops/removes only the proven-owned program and
restores the original event/keepalive settings only when they still match the
applied settings. Failed or interrupted restoration retains a recovery record
instead of overwriting someone else's configuration. After a restart, a disabled
reader with unfinished claims runs cleanup only: it never creates or starts a
new RF program. Actual absence of the claimed program releases its pending
record. A worker whose configuration generation changes cleans up and exits
instead of reconnecting with stale antenna settings.

Reader UTC and server UTC are different clock domains. The adapter samples the
reader clock against the local request/connect start and conservatively converts
emitted tag and coverage times before applying server capture or manual fences.
Raw reader times remain protocol-order/replay evidence only. This does not add
the full allowed clock-skew interval to healthy enrollment. A detected forward
clock change requires reinspection before accepting more evidence.

Catalog changes can supersede in-flight runtime evidence. A rejected old batch
is discarded, never replayed. The live worker reinspects the reader and earns a
fresh epoch and warm-up before resuming coverage; storage failures remain errors.

Offline wire replay produces explicitly simulated evidence through the same
normalization contract. It does not connect to hardware or promote simulation
into production tracking. Live RF commissioning is still required with the
actual reader, final antenna installation and tagged garments.
