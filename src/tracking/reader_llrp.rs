//! Small inventory-only LLRP 1.0.1 client for R700. No AccessSpec encoder exists.
//!
//! GS1 LLRP sections 11, 14 and 17 define the binary framing and AISpec end
//! event. Impinj's R700 Octane 10.4 manual confirms 32 AISpecs, one ROSpec,
//! antenna connectivity and TCP-only transport. A completed, duration-controlled
//! single-port AISpec can prove that port's interval; a keepalive cannot.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::Duration,
};
use tokio::{net::TcpStream, sync::watch};
use uuid::Uuid;

use crate::store::Store;
use crate::tracking::{
    models::*,
    reader::{
        ReaderClock, ReaderError, ReaderTimeBasis, ReconnectBackoff, admit_disconnect, admit_live,
        hex,
    },
};

const MAX_MESSAGE: usize = 524_288;
const HEADER: usize = 10;
const IO_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub kind: u16,
    pub id: u32,
    pub body: Vec<u8>,
}
impl Message {
    pub fn encode(&self) -> Result<Vec<u8>, ReaderError> {
        let size = self
            .body
            .len()
            .checked_add(HEADER)
            .filter(|n| *n <= MAX_MESSAGE)
            .ok_or(ReaderError::Oversized)?;
        if self.kind > 1_023 {
            return Err(ReaderError::Protocol);
        }
        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(&(0x0400 | self.kind).to_be_bytes()); // version1 = LLRP1.0.1
        bytes.extend_from_slice(
            &u32::try_from(size)
                .map_err(|_| ReaderError::Oversized)?
                .to_be_bytes(),
        );
        bytes.extend_from_slice(&self.id.to_be_bytes());
        bytes.extend_from_slice(&self.body);
        Ok(bytes)
    }
}
#[derive(Default)]
pub struct Framer {
    bytes: Vec<u8>,
}
impl Framer {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Message>, ReaderError> {
        if bytes.len() > MAX_MESSAGE
            || self.bytes.len().saturating_add(bytes.len()) > MAX_MESSAGE * 2
        {
            return Err(ReaderError::Oversized);
        }
        self.bytes.extend_from_slice(bytes);
        let mut messages = Vec::new();
        while self.bytes.len() >= HEADER {
            let prefix = be16(&self.bytes, 0)?;
            if prefix >> 10 != 1 {
                return Err(ReaderError::Protocol);
            }
            let length =
                usize::try_from(be32(&self.bytes, 2)?).map_err(|_| ReaderError::Protocol)?;
            if !(HEADER..=MAX_MESSAGE).contains(&length) {
                return Err(ReaderError::Oversized);
            }
            if self.bytes.len() < length {
                break;
            }
            messages.push(Message {
                kind: prefix & 0x03ff,
                id: be32(&self.bytes, 6)?,
                body: self.bytes[HEADER..length].to_vec(),
            });
            self.bytes.drain(..length);
        }
        Ok(messages)
    }
}

#[derive(Clone, Copy, Debug)]
struct Parameter<'a> {
    kind: u16,
    body: &'a [u8],
    whole: &'a [u8],
}
fn parameters(mut bytes: &[u8]) -> Result<Vec<Parameter<'_>>, ReaderError> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let (kind, header, length) = if bytes[0] & 0x80 != 0 {
            let kind = u16::from(bytes[0] & 0x7f);
            let length = match kind {
                1 | 7 | 8 | 10 | 11 | 12 | 14 | 15 | 17 => 3,
                2..=5 => 9,
                6 => 2,
                9 | 16 => 5,
                13 => 13,
                _ => return Err(ReaderError::Protocol),
            };
            (kind, 1, length)
        } else {
            if bytes.len() < 4 || be16(bytes, 0)? & 0xfc00 != 0 {
                return Err(ReaderError::Protocol);
            }
            (be16(bytes, 0)?, 4, usize::from(be16(bytes, 2)?))
        };
        if length < header || length > bytes.len() {
            return Err(ReaderError::Protocol);
        }
        out.push(Parameter {
            kind,
            body: &bytes[header..length],
            whole: &bytes[..length],
        });
        if out.len() > 8_192 {
            return Err(ReaderError::Oversized);
        }
        bytes = &bytes[length..];
    }
    Ok(out)
}
fn tlv(kind: u16, body: &[u8]) -> Result<Vec<u8>, ReaderError> {
    let length = u16::try_from(body.len().checked_add(4).ok_or(ReaderError::Oversized)?)
        .map_err(|_| ReaderError::Oversized)?;
    let mut result = Vec::with_capacity(usize::from(length));
    result.extend(kind.to_be_bytes());
    result.extend(length.to_be_bytes());
    result.extend(body);
    Ok(result)
}
fn be16(bytes: &[u8], offset: usize) -> Result<u16, ReaderError> {
    Ok(u16::from_be_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or(ReaderError::Protocol)?
            .try_into()
            .map_err(|_| ReaderError::Protocol)?,
    ))
}
fn be32(bytes: &[u8], offset: usize) -> Result<u32, ReaderError> {
    Ok(u32::from_be_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(ReaderError::Protocol)?
            .try_into()
            .map_err(|_| ReaderError::Protocol)?,
    ))
}
fn be64(bytes: &[u8], offset: usize) -> Result<u64, ReaderError> {
    Ok(u64::from_be_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or(ReaderError::Protocol)?
            .try_into()
            .map_err(|_| ReaderError::Protocol)?,
    ))
}
fn timestamp(bytes: &[u8]) -> Result<DateTime<Utc>, ReaderError> {
    DateTime::from_timestamp_micros(
        i64::try_from(be64(bytes, 0)?).map_err(|_| ReaderError::Protocol)?,
    )
    .ok_or(ReaderError::Protocol)
}
fn successful(message: &Message) -> Result<(), ReaderError> {
    let payload = if message.kind == 1023 {
        if be32(&message.body, 0)? != 25_882 || message.body.get(4) != Some(&22) {
            return Err(ReaderError::Protocol);
        }
        message.body.get(5..).ok_or(ReaderError::Protocol)?
    } else {
        &message.body
    };
    let params = parameters(payload)?;
    let status = params
        .iter()
        .find(|p| p.kind == 287)
        .ok_or(ReaderError::Protocol)?;
    if be16(status.body, 0)? != 0 {
        return Err(ReaderError::Unsupported);
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct ScanPort {
    config: PortConfig,
    dwell: u32,
    transmitter: Vec<u8>,
}
fn impinj(subtype: u32, body: &[u8]) -> Result<Vec<u8>, ReaderError> {
    let mut bytes = 25_882u32.to_be_bytes().to_vec();
    bytes.extend(subtype.to_be_bytes());
    bytes.extend(body);
    tlv(1023, &bytes)
}
fn rospec(id: u32, ports: &[ScanPort]) -> Result<Vec<u8>, ReaderError> {
    if id == 0 || ports.is_empty() || ports.len() > 32 {
        return Err(ReaderError::Configuration);
    }
    let mut body = id.to_be_bytes().to_vec();
    body.extend([0, 0]); // priority0, Disabled
    let mut boundary = tlv(179, &[0])?; // explicit START_ROSPEC, never auto-start
    boundary.extend(tlv(182, &[0, 0, 0, 0, 0])?); // no premature ROSpec stop
    body.extend(tlv(178, &boundary)?);
    for (index, port) in ports.iter().enumerate() {
        if port.dwell == 0 || port.dwell > 60_000 {
            return Err(ReaderError::Configuration);
        }
        let mut ai = 1u16.to_be_bytes().to_vec();
        ai.extend(port.config.antenna.to_be_bytes());
        let mut stop = vec![1];
        stop.extend(port.dwell.to_be_bytes()); // Duration
        ai.extend(tlv(184, &stop)?);
        let mut inventory = u16::try_from(index + 1)
            .map_err(|_| ReaderError::Configuration)?
            .to_be_bytes()
            .to_vec();
        inventory.push(1); // EPC Gen2
        let mut antenna = port.config.antenna.to_be_bytes().to_vec();
        antenna.extend(&port.transmitter);
        // Explicitly prevent a global tag-suppression / low-duty / skip-empty
        // setting from turning a duration completion into false RF coverage.
        let mut command = vec![0]; // state-unaware singulation, supported by R700
        let mut singulation = vec![0x80];
        singulation.extend(100u16.to_be_bytes());
        singulation.extend(0u32.to_be_bytes());
        command.extend(tlv(336, &singulation)?); // Session2, initial population100
        command.extend(impinj(23, &6u16.to_be_bytes())?); // DualTarget B-to-A Select
        command.extend(impinj(28, &[0; 6])?); // LowDutyCycle disabled
        command.extend(impinj(1554, &[0])?); // IntelligentAntennaManagement disabled
        antenna.extend(tlv(330, &command)?);
        inventory.extend(tlv(222, &antenna)?);
        ai.extend(tlv(186, &inventory)?);
        body.extend(tlv(183, &ai)?);
    }
    let mut report = vec![1];
    report.extend(64u16.to_be_bytes()); // n tags OR end of AISpec
    // ROSpec, SpecIndex, InventoryParameterSpecID, antenna, RSSI, UTC first/last,
    // count. No AccessSpec or tag-memory selectors.
    report.extend(tlv(238, &0xf780u16.to_be_bytes())?);
    body.extend(tlv(237, &report)?);
    tlv(177, &body)
}
fn rospec_fingerprint(bytes: &[u8]) -> Result<String, ReaderError> {
    let parsed = parameters(bytes)?;
    if parsed.len() != 1 || parsed[0].kind != 177 || parsed[0].body.len() < 6 {
        return Err(ReaderError::Protocol);
    }
    let mut canonical = bytes.to_vec();
    canonical[9] = 0; // CurrentState alone is runtime state.
    Ok(hex(&Sha256::digest(&canonical)).to_ascii_lowercase())
}

/// A decoder is armed only after the new START response and a fresh reader
/// ROSpecStart timestamp. Old buffered packets cannot establish that barrier.
struct CycleDecoder {
    reader: ReaderId,
    owned: u32,
    ports: Vec<ScanPort>,
    session: Uuid,
    sequence: u64,
    armed_after: DateTime<Utc>,
    started: Option<DateTime<Utc>>,
    next_spec: usize,
    interval_start: Option<DateTime<Utc>>,
    connected: BTreeSet<u16>,
    failed: bool,
    ended: bool,
    data_source: DataSource,
    latest_reads: BTreeMap<(PortId, Epc), DateTime<Utc>>,
    last_event_at: Option<DateTime<Utc>>,
    envelope_at: Option<DateTime<Utc>>,
    time_basis: ReaderTimeBasis,
}

/// Offline wire replay emits only Simulation evidence and performs no I/O.
/// Supplying connected antennas is part of the fictional capture's baseline,
/// not a live antenna-health override.
pub struct LlrpReplay {
    framer: Framer,
    decoder: CycleDecoder,
}
impl LlrpReplay {
    pub fn new(
        reader: ReaderId,
        ports: &[PortConfig],
        owned_rospec_id: u32,
        reader_fence: DateTime<Utc>,
        connected: BTreeSet<u16>,
        session: Uuid,
    ) -> Result<Self, ReaderError> {
        let mut antennas = BTreeSet::new();
        let ports: Vec<ScanPort> = ports
            .iter()
            .filter(|p| p.reader_id == reader && p.enabled)
            .map(|p| {
                if p.antenna == 0 || !antennas.insert(p.antenna) {
                    return Err(ReaderError::Configuration);
                }
                Ok(ScanPort {
                    config: p.clone(),
                    dwell: p.dwell_ms.unwrap_or(250),
                    transmitter: Vec::new(),
                })
            })
            .collect::<Result<_, ReaderError>>()?;
        if ports.is_empty() || ports.len() > 32 || owned_rospec_id == 0 || session.is_nil() {
            return Err(ReaderError::Configuration);
        }
        Ok(Self {
            framer: Framer::default(),
            decoder: CycleDecoder {
                reader,
                owned: owned_rospec_id,
                ports,
                session,
                sequence: 0,
                armed_after: reader_fence,
                started: None,
                next_spec: 0,
                interval_start: None,
                connected,
                failed: false,
                ended: false,
                data_source: DataSource::Simulation,
                latest_reads: BTreeMap::new(),
                last_event_at: None,
                envelope_at: None,
                time_basis: ReaderTimeBasis::default(),
            },
        })
    }
    pub fn push(
        &mut self,
        bytes: &[u8],
        received_at: DateTime<Utc>,
    ) -> Result<Vec<ObservationBatch>, ReaderError> {
        let mut batches = Vec::new();
        for message in self.framer.push(bytes)? {
            let batch = match message.kind {
                63 => self.decoder.event(&message, received_at)?,
                61 => self.decoder.reports(&message, received_at)?,
                62 => None,
                _ => return Err(ReaderError::Protocol),
            };
            if let Some(batch) = batch {
                batches.push(batch);
            }
        }
        Ok(batches)
    }
}
impl CycleDecoder {
    fn batch(&mut self, at: DateTime<Utc>) -> Result<ObservationBatch, ReaderError> {
        self.sequence = self.sequence.checked_add(1).ok_or(ReaderError::Protocol)?;
        let last = self
            .last_event_at
            .map(|at| self.time_basis.server_time(at))
            .transpose()?;
        let at = at
            .max(last.unwrap_or(at))
            .max(self.envelope_at.unwrap_or(at));
        self.envelope_at = Some(at);
        Ok(ObservationBatch {
            reader_id: self.reader.clone(),
            session: self.session,
            sequence: self.sequence,
            observed_at: at,
            connected: true,
            coverage: Vec::new(),
            observations: Vec::new(),
            purpose: ObservationPurpose::Inventory,
            data_source: self.data_source,
            capture_session: None,
            readback: None,
        })
    }
    fn arm(&mut self, after: DateTime<Utc>) {
        self.armed_after = after;
        self.started = None;
        self.interval_start = None;
        self.next_spec = 0;
        self.failed = false;
        self.ended = false;
        self.latest_reads.clear();
    }
    fn event(
        &mut self,
        message: &Message,
        now: DateTime<Utc>,
    ) -> Result<Option<ObservationBatch>, ReaderError> {
        let outer = parameters(&message.body)?;
        let data = outer
            .iter()
            .find(|p| p.kind == 246)
            .ok_or(ReaderError::Protocol)?;
        let params = parameters(data.body)?;
        let at = timestamp(
            params
                .iter()
                .find(|p| p.kind == 128)
                .ok_or(ReaderError::Unsupported)?
                .body,
        )?;
        if at < self.armed_after
            || at > now + chrono::Duration::seconds(2)
            || now - at > chrono::Duration::seconds(60)
        {
            return Ok(None);
        }
        if self.last_event_at.is_some_and(|previous| at < previous) {
            return Err(ReaderError::Protocol);
        }
        let normalized_at = self.time_basis.observation_time(at, now)?;
        self.last_event_at = Some(at);
        let mut batch = self.batch(normalized_at)?;
        for p in params.iter().filter(|p| p.kind != 128) {
            match p.kind {
                249 => {
                    let kind = *p.body.first().ok_or(ReaderError::Protocol)?;
                    if be32(p.body, 1)? != self.owned {
                        self.failed = true;
                        return Err(ReaderError::Protocol);
                    }
                    match kind {
                        0 => {
                            if at <= self.armed_after {
                                return Ok(None);
                            }
                            if self.started.is_some() {
                                self.failed = true;
                                return Err(ReaderError::Protocol);
                            }
                            self.started = Some(at);
                            self.interval_start = Some(at);
                            self.next_spec = 0;
                        }
                        1 if self.next_spec == self.ports.len() && !self.failed => {
                            self.ended = true;
                        }
                        1 | 2 => {
                            self.failed = true;
                            return Err(ReaderError::Protocol);
                        }
                        _ => return Err(ReaderError::Protocol),
                    }
                }
                254 => {
                    if p.body.first() != Some(&0) || be32(p.body, 1)? != self.owned {
                        return Err(ReaderError::Protocol);
                    }
                    let index = usize::from(be16(p.body, 5)?);
                    if self.failed || self.started.is_none() || index != self.next_spec + 1 {
                        return Err(ReaderError::Protocol);
                    }
                    let port = self
                        .ports
                        .get(self.next_spec)
                        .ok_or(ReaderError::Protocol)?;
                    let start = self.interval_start.ok_or(ReaderError::Protocol)?;
                    if at.signed_duration_since(start).num_milliseconds() < i64::from(port.dwell) {
                        return Err(ReaderError::Protocol);
                    }
                    let proof = if self.connected.contains(&port.config.antenna) {
                        CoverageProof::VerifiedInventory
                    } else {
                        CoverageProof::Fault {
                            code: ReaderFault::AntennaFault,
                        }
                    };
                    batch.coverage.push(PortCoverage {
                        port_id: port.config.id.clone(),
                        started_at: self.time_basis.server_time(start)?,
                        ended_at: self.time_basis.server_time(at)?,
                        proof,
                    });
                    self.interval_start = Some(at);
                    self.next_spec += 1;
                }
                255 => {
                    let antenna = be16(p.body, 1)?;
                    match p.body.first() {
                        Some(1) => {
                            self.connected.insert(antenna);
                        }
                        Some(0) => {
                            self.connected.remove(&antenna);
                            self.failed = true;
                        }
                        _ => return Err(ReaderError::Protocol),
                    }
                    if let Some(port) = self
                        .ports
                        .iter()
                        .find(|port| port.config.antenna == antenna)
                    {
                        batch.coverage.push(PortCoverage {
                            port_id: port.config.id.clone(),
                            started_at: self.time_basis.server_time(at)?,
                            ended_at: self.time_basis.server_time(at)?,
                            proof: if self.connected.contains(&antenna) {
                                CoverageProof::TransportOnly
                            } else {
                                CoverageProof::Fault {
                                    code: ReaderFault::AntennaFault,
                                }
                            },
                        });
                    }
                }
                250 | 251 | 252 | 257 => {
                    self.failed = true;
                    return Err(ReaderError::Protocol);
                }
                256 if be16(p.body, 0)? != 0 => return Err(ReaderError::Transport),
                129 => return Err(ReaderError::Unsupported), // uptime is not a UTC barrier
                _ => {}
            }
        }
        Ok(Some(batch))
    }
    fn reports(
        &mut self,
        message: &Message,
        now: DateTime<Utc>,
    ) -> Result<Option<ObservationBatch>, ReaderError> {
        let Some(started) = self.started else {
            return Ok(None);
        };
        if self.failed {
            return Ok(None);
        }
        let mut batch = self.batch(now)?;
        for report in parameters(&message.body)?.iter().filter(|p| p.kind == 240) {
            if batch.observations.len() == MAX_BATCH_OBSERVATIONS {
                return Err(ReaderError::Oversized);
            }
            let fields = parameters(report.body)?;
            let field = |kind| {
                fields
                    .iter()
                    .find(|p| p.kind == kind)
                    .map(|p| p.body)
                    .ok_or(ReaderError::Protocol)
            };
            if be32(field(9)?, 0)? != self.owned {
                continue;
            }
            let index = usize::from(be16(field(14)?, 0)?);
            if index != self.next_spec + 1 || index == 0 {
                return Err(ReaderError::Protocol);
            }
            if be16(field(10)?, 0)? != u16::try_from(index).map_err(|_| ReaderError::Protocol)? {
                return Err(ReaderError::Protocol);
            }
            let port = self.ports.get(index - 1).ok_or(ReaderError::Protocol)?;
            if be16(field(1)?, 0)? != port.config.antenna {
                return Err(ReaderError::Protocol);
            }
            let first = timestamp(field(2)?)?;
            let last = timestamp(field(4)?)?;
            if first < started
                || last < first
                || last > now + chrono::Duration::seconds(2)
                || now - last > chrono::Duration::seconds(60)
            {
                continue;
            }
            let epc = if let Some(epc) = fields.iter().find(|p| p.kind == 13) {
                hex(epc.body)
                    .parse::<Epc>()
                    .map_err(|_| ReaderError::Protocol)?
            } else {
                let data = field(241)?;
                let bits = usize::from(be16(data, 0)?);
                if bits == 0 || !bits.is_multiple_of(8) || data.len() != 2 + bits / 8 {
                    return Err(ReaderError::Protocol);
                }
                hex(&data[2..])
                    .parse::<Epc>()
                    .map_err(|_| ReaderError::Protocol)?
            };
            let count = u32::from(be16(field(8)?, 0)?);
            if count == 0 {
                return Err(ReaderError::Protocol);
            }
            let key = (port.config.id.clone(), epc.clone());
            if self
                .latest_reads
                .get(&key)
                .is_some_and(|prior| last <= *prior)
            {
                continue;
            }
            if self.latest_reads.len() >= MAX_BATCH_OBSERVATIONS
                && !self.latest_reads.contains_key(&key)
            {
                return Err(ReaderError::Oversized);
            }
            let normalized_first = self.time_basis.observation_time(first, now)?;
            let normalized_last = self.time_basis.observation_time(last, now)?;
            self.latest_reads.insert(key, last);
            self.last_event_at = Some(last.max(self.last_event_at.unwrap_or(last)));
            batch.observed_at = batch.observed_at.max(normalized_last);
            let peak = fields
                .iter()
                .find(|p| p.kind == 6)
                .and_then(|p| p.body.first())
                .map(|n| i32::from(i8::from_be_bytes([*n])) * 100);
            let event_ref = format!(
                "llrp:{}:{}:{}:{}:{}",
                self.owned,
                index,
                epc,
                first.timestamp_micros(),
                last.timestamp_micros()
            );
            batch.observations.push(TagObservation {
                epc,
                port_id: port.config.id.clone(),
                first_seen_at: normalized_first,
                last_seen_at: normalized_last,
                read_count: count,
                tid: None,
                peak_rssi_cdbm: peak,
                event_ref: Some(event_ref),
            });
        }
        Ok(Some(batch))
    }
}

struct Connection {
    socket: TcpStream,
    framer: Framer,
    pending: VecDeque<Message>,
    id: u32,
}
impl Connection {
    async fn connect(host: &str, port: u16) -> Result<Self, ReaderError> {
        let socket = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect((host, port)))
            .await
            .map_err(|_| ReaderError::Transport)?
            .map_err(|_| ReaderError::Transport)?;
        socket
            .set_nodelay(true)
            .map_err(|_| ReaderError::Transport)?;
        Ok(Self {
            socket,
            framer: Framer::default(),
            pending: VecDeque::new(),
            id: 0,
        })
    }
    async fn send(&mut self, kind: u16, id: u32, body: Vec<u8>) -> Result<(), ReaderError> {
        let bytes = Message { kind, id, body }.encode()?;
        tokio::time::timeout(IO_TIMEOUT, async {
            let mut position = 0;
            while position < bytes.len() {
                self.socket
                    .writable()
                    .await
                    .map_err(|_| ReaderError::Transport)?;
                match self.socket.try_write(&bytes[position..]) {
                    Ok(0) => return Err(ReaderError::Transport),
                    Ok(n) => position += n,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => return Err(ReaderError::Transport),
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| ReaderError::Transport)?
    }
    async fn next(&mut self) -> Result<Message, ReaderError> {
        loop {
            if let Some(message) = self.pending.pop_front() {
                return Ok(message);
            }
            self.socket
                .readable()
                .await
                .map_err(|_| ReaderError::Transport)?;
            let mut bytes = [0u8; 16_384];
            match self.socket.try_read(&mut bytes) {
                Ok(0) => return Err(ReaderError::Transport),
                Ok(n) => self.pending.extend(self.framer.push(&bytes[..n])?),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return Err(ReaderError::Transport),
            }
        }
    }
    async fn request(
        &mut self,
        kind: u16,
        response: u16,
        body: Vec<u8>,
    ) -> Result<Message, ReaderError> {
        self.id = self.id.checked_add(1).ok_or(ReaderError::Protocol)?;
        let id = self.id;
        self.send(kind, id, body).await?;
        let mut deferred = VecDeque::new();
        let result = tokio::time::timeout(IO_TIMEOUT, async {
            loop {
                let message = self.next().await?;
                if message.kind == 62 {
                    self.send(72, message.id, Vec::new()).await?;
                    continue;
                }
                if message.id == id && (message.kind == response || message.kind == 100) {
                    successful(&message)?;
                    return Ok(message);
                }
                if !matches!(message.kind, 61 | 63) || deferred.len() >= 256 {
                    return Err(ReaderError::Protocol);
                }
                deferred.push_back(message);
            }
        })
        .await
        .map_err(|_| ReaderError::Transport)?;
        while let Some(message) = deferred.pop_back() {
            self.pending.push_front(message);
        }
        result
    }
    async fn config(&mut self) -> Result<Message, ReaderError> {
        self.request(2, 12, vec![0; 7]).await
    }
    async fn control_readback(&mut self) -> Result<LlrpControlSettings, ReaderError> {
        let mut body = Vec::new();
        for requested in [5, 8, 11] {
            let mut request = vec![0; 7];
            request[2] = requested;
            let response = self.request(2, 12, request).await?;
            for parameter in parameters(&response.body)?
                .iter()
                .filter(|p| matches!(p.kind, 244 | 220 | 226))
            {
                body.extend(parameter.whole);
            }
        }
        control_settings(&Message {
            kind: 12,
            id: 0,
            body,
        })
    }
    async fn programs(&mut self) -> Result<Vec<Vec<u8>>, ReaderError> {
        let response = self.request(26, 36, Vec::new()).await?;
        parameters(&response.body)?
            .into_iter()
            .filter(|p| p.kind == 177)
            .map(|p| {
                be32(p.body, 0)?;
                Ok(p.whole.to_vec())
            })
            .collect()
    }
    async fn owned(&mut self, id: u32) -> Result<Option<Vec<u8>>, ReaderError> {
        let programs = self.programs().await?;
        if programs.iter().any(|p| be32(p, 4).ok() != Some(id)) || programs.len() > 1 {
            return Err(ReaderError::Unsupported);
        }
        Ok(programs.into_iter().next())
    }
    async fn owned_for_cleanup(&mut self, id: u32) -> Result<Option<Vec<u8>>, ReaderError> {
        let mut matching = self
            .programs()
            .await?
            .into_iter()
            .filter(|p| be32(p, 4).ok() == Some(id));
        let owned = matching.next();
        if matching.next().is_some() {
            return Err(ReaderError::Protocol);
        }
        Ok(owned)
    }
    async fn enable_extensions(&mut self) -> Result<(), ReaderError> {
        let mut enable = 25_882u32.to_be_bytes().to_vec();
        enable.push(21);
        enable.extend([0; 4]);
        self.request(1023, 1023, enable).await?;
        Ok(())
    }
}

fn control_settings(message: &Message) -> Result<LlrpControlSettings, ReaderError> {
    let params = parameters(&message.body)?;
    let notification = params
        .iter()
        .find(|p| p.kind == 244)
        .ok_or(ReaderError::Unsupported)?;
    let mut notifications = Vec::new();
    let mut seen = BTreeSet::new();
    for p in parameters(notification.body)? {
        if p.kind != 245 {
            return Err(ReaderError::Unsupported);
        }
        let event_type = be16(p.body, 0)?;
        let enabled = *p.body.get(2).ok_or(ReaderError::Protocol)?;
        if enabled & 0x7f != 0 || !seen.insert(event_type) {
            return Err(ReaderError::Protocol);
        }
        notifications.push(LlrpEventSetting {
            event_type,
            enabled: enabled & 0x80 != 0,
        });
    }
    notifications.sort_by_key(|item| item.event_type);
    let keepalive = params
        .iter()
        .find(|p| p.kind == 220)
        .ok_or(ReaderError::Unsupported)?;
    let trigger = match keepalive.body.first() {
        Some(0) => KeepaliveTrigger::None,
        Some(1) => KeepaliveTrigger::Periodic,
        _ => return Err(ReaderError::Protocol),
    };
    let events = params
        .iter()
        .find(|p| p.kind == 226)
        .ok_or(ReaderError::Unsupported)?;
    let hold = *events.body.first().ok_or(ReaderError::Protocol)?;
    if hold & 0x7f != 0 {
        return Err(ReaderError::Protocol);
    }
    Ok(LlrpControlSettings {
        notifications,
        keepalive: LlrpKeepalive {
            trigger,
            interval_ms: be32(keepalive.body, 1)?,
        },
        hold_events_and_reports_upon_reconnect: hold & 0x80 != 0,
    })
}
fn desired_control(current: &LlrpControlSettings) -> LlrpControlSettings {
    let mut desired = current.clone();
    for event_type in [2, 3, 4, 6, 8] {
        if let Some(item) = desired
            .notifications
            .iter_mut()
            .find(|item| item.event_type == event_type)
        {
            item.enabled = true;
        } else {
            desired.notifications.push(LlrpEventSetting {
                event_type,
                enabled: true,
            });
        }
    }
    desired.notifications.sort_by_key(|item| item.event_type);
    desired.keepalive = LlrpKeepalive {
        trigger: KeepaliveTrigger::Periodic,
        interval_ms: 2_000,
    };
    desired.hold_events_and_reports_upon_reconnect = false;
    desired
}
fn encode_control(settings: &LlrpControlSettings) -> Result<Vec<u8>, ReaderError> {
    let mut body = vec![0]; // ResetToFactoryDefaults=false
    let mut events = Vec::new();
    for item in &settings.notifications {
        let mut value = item.event_type.to_be_bytes().to_vec();
        value.push(if item.enabled { 0x80 } else { 0 });
        events.extend(tlv(245, &value)?);
    }
    body.extend(tlv(244, &events)?);
    let mut keepalive = vec![match settings.keepalive.trigger {
        KeepaliveTrigger::None => 0,
        KeepaliveTrigger::Periodic => 1,
    }];
    keepalive.extend(settings.keepalive.interval_ms.to_be_bytes());
    body.extend(tlv(220, &keepalive)?);
    body.extend(tlv(
        226,
        &[if settings.hold_events_and_reports_upon_reconnect {
            0x80
        } else {
            0
        }],
    )?);
    Ok(body)
}

struct Capabilities {
    firmware: String,
    max_antenna: u16,
    max_specs: usize,
    powers: BTreeMap<u16, i32>,
}
fn capabilities(message: &Message) -> Result<Capabilities, ReaderError> {
    let params = parameters(&message.body)?;
    let general = params
        .iter()
        .find(|p| p.kind == 137)
        .ok_or(ReaderError::Unsupported)?;
    let maximum = be16(general.body, 0)?;
    if !(1..=32).contains(&maximum) || general.body.get(2).is_none_or(|flags| flags & 0x40 == 0) {
        return Err(ReaderError::Unsupported);
    }
    if be32(general.body, 4)? != 25_882 || !matches!(be32(general.body, 8)?, 2_001_052 | 2_001_070)
    {
        return Err(ReaderError::Unsupported);
    }
    let version_length = usize::from(be16(general.body, 12)?);
    let firmware = std::str::from_utf8(
        general
            .body
            .get(14..14 + version_length)
            .ok_or(ReaderError::Protocol)?,
    )
    .map_err(|_| ReaderError::Protocol)?
    .to_owned();
    let regulatory = params
        .iter()
        .find(|p| p.kind == 143)
        .ok_or(ReaderError::Unsupported)?;
    let region = parameters(regulatory.body.get(4..).ok_or(ReaderError::Protocol)?)?;
    let band = region
        .iter()
        .find(|p| p.kind == 144)
        .ok_or(ReaderError::Unsupported)?;
    let mut powers = BTreeMap::new();
    for p in parameters(band.body)?.iter().filter(|p| p.kind == 145) {
        let index = be16(p.body, 0)?;
        let power = i32::from(i16::from_be_bytes(be16(p.body, 2)?.to_be_bytes()));
        if index == 0 || powers.insert(index, power).is_some() {
            return Err(ReaderError::Protocol);
        }
    }
    if powers.is_empty() {
        return Err(ReaderError::Unsupported);
    }
    let llrp = params
        .iter()
        .find(|p| p.kind == 142)
        .ok_or(ReaderError::Unsupported)?;
    let max_specs = usize::try_from(be32(llrp.body, 8)?).map_err(|_| ReaderError::Unsupported)?;
    if be32(llrp.body, 4)? < 1 || be32(llrp.body, 12)? < 1 || max_specs == 0 {
        return Err(ReaderError::Unsupported);
    }
    Ok(Capabilities {
        firmware,
        max_antenna: maximum,
        max_specs,
        powers,
    })
}
fn scan_ports(
    config: &Message,
    caps: &Capabilities,
    ports: &[PortConfig],
    reader: &ReaderId,
) -> Result<(Vec<ScanPort>, BTreeSet<u16>), ReaderError> {
    let params = parameters(&config.body)?;
    let mut scans = Vec::new();
    let mut connected = BTreeSet::new();
    for p in params.iter().filter(|p| p.kind == 221) {
        if p.body.first().is_some_and(|b| b & 0x80 != 0) {
            connected.insert(be16(p.body, 1)?);
        }
    }
    for port in ports.iter().filter(|p| p.reader_id == *reader && p.enabled) {
        if port.antenna == 0 || port.antenna > caps.max_antenna {
            return Err(ReaderError::Configuration);
        }
        let antenna = params
            .iter()
            .find(|p| p.kind == 222 && be16(p.body, 0).ok() == Some(port.antenna))
            .ok_or(ReaderError::Unsupported)?;
        let fields = parameters(antenna.body.get(2..).ok_or(ReaderError::Protocol)?)?;
        let transmitter = fields
            .iter()
            .find(|p| p.kind == 224)
            .ok_or(ReaderError::Unsupported)?;
        if transmitter.body.len() != 6 {
            return Err(ReaderError::Protocol);
        }
        let mut transmitter_body = transmitter.body.to_vec();
        if let Some(requested) = port.transmit_power_cdbm {
            let index = caps
                .powers
                .iter()
                .find(|(_, power)| **power == requested)
                .map(|(index, _)| *index)
                .ok_or(ReaderError::Configuration)?;
            transmitter_body[4..6].copy_from_slice(&index.to_be_bytes());
        }
        if !caps.powers.contains_key(&be16(&transmitter_body, 4)?) {
            return Err(ReaderError::Protocol);
        }
        scans.push(ScanPort {
            config: port.clone(),
            dwell: port.dwell_ms.unwrap_or(250),
            transmitter: tlv(224, &transmitter_body)?,
        });
    }
    if scans.is_empty() {
        return Err(ReaderError::Configuration);
    }
    if scans.len() > caps.max_specs {
        return Err(ReaderError::Unsupported);
    }
    Ok((scans, connected))
}

async fn apply_control(
    connection: &mut Connection,
    store: &Store,
    reader: &ReaderId,
    version: i64,
    clock: &dyn ReaderClock,
) -> Result<(), ReaderError> {
    let current = control_settings(&connection.config().await?)?;
    let desired = desired_control(&current);
    let plan = store
        .tracking_reader_plan_control_clocked(reader, version, &current, &desired, || clock.now())
        .await
        .map_err(|error| ReaderError::State(error.code()))?;
    if plan.action == ReaderPlanAction::Recover {
        return Err(ReaderError::Unsupported);
    }
    if plan.action != ReaderPlanAction::Keep {
        connection
            .request(3, 13, encode_control(&plan.target)?)
            .await?;
    }
    let readback = control_settings(&connection.config().await?)?;
    if readback != plan.target {
        return Err(ReaderError::Protocol);
    }
    store
        .tracking_reader_control_applied_clocked(reader, version, &readback, false, || clock.now())
        .await
        .map_err(|error| ReaderError::State(error.code()))?;
    Ok(())
}
async fn restore_control(
    connection: &mut Connection,
    store: &Store,
    reader: &ReaderId,
    clock: &dyn ReaderClock,
) -> Result<(), ReaderError> {
    let current = control_settings(&connection.config().await?)?;
    let plan = store
        .tracking_reader_restore_control_clocked(reader, &current, || clock.now())
        .await
        .map_err(|error| ReaderError::State(error.code()))?;
    if plan.action == ReaderPlanAction::Recover {
        return Err(ReaderError::Unsupported);
    }
    if plan.action != ReaderPlanAction::Keep {
        connection
            .request(3, 13, encode_control(&plan.target)?)
            .await?;
    }
    let readback = control_settings(&connection.config().await?)?;
    if readback != plan.target {
        return Err(ReaderError::Protocol);
    }
    store
        .tracking_reader_control_applied_clocked(
            reader,
            plan.operation.configuration_version,
            &readback,
            true,
            || clock.now(),
        )
        .await
        .map_err(|error| ReaderError::State(error.code()))?;
    Ok(())
}
async fn stop_owned(
    connection: &mut Connection,
    id: u32,
    expected: &str,
) -> Result<(), ReaderError> {
    if let Some(owned) = connection.owned(id).await? {
        if rospec_fingerprint(&owned)? != expected {
            return Err(ReaderError::Unsupported);
        }
        let state = *owned.get(9).ok_or(ReaderError::Protocol)?;
        if state == 2 {
            connection
                .request(23, 33, id.to_be_bytes().to_vec())
                .await?;
        }
        if state != 0 {
            connection
                .request(25, 35, id.to_be_bytes().to_vec())
                .await?;
        }
        let readback = connection.owned(id).await?.ok_or(ReaderError::Protocol)?;
        if rospec_fingerprint(&readback)? != expected || readback.get(9) != Some(&0) {
            return Err(ReaderError::Protocol);
        }
    }
    Ok(())
}
async fn configure_owned(
    connection: &mut Connection,
    store: &Store,
    reader: &ReaderId,
    version: i64,
    id: u32,
    desired: &[u8],
    clock: &dyn ReaderClock,
) -> Result<String, ReaderError> {
    let fingerprint = rospec_fingerprint(desired)?;
    let observed = connection.owned(id).await?;
    let observed_fingerprint = observed
        .as_ref()
        .map(|bytes| rospec_fingerprint(bytes))
        .transpose()?;
    let plan = store
        .tracking_reader_plan_clocked(
            reader,
            version,
            &fingerprint,
            observed_fingerprint.as_deref(),
            || clock.now(),
        )
        .await
        .map_err(|error| ReaderError::State(error.code()))?;
    if plan.action == ReaderPlanAction::Recover {
        return Err(ReaderError::Unsupported);
    }
    if let Some(observed_fingerprint) = observed_fingerprint {
        stop_owned(connection, id, &observed_fingerprint).await?;
        if plan.action == ReaderPlanAction::Replace {
            connection
                .request(21, 31, id.to_be_bytes().to_vec())
                .await?;
        }
    }
    apply_control(connection, store, reader, version, clock).await?;
    if matches!(
        plan.action,
        ReaderPlanAction::Create | ReaderPlanAction::Replace
    ) {
        connection.request(20, 30, desired.to_vec()).await?;
    }
    let readback = connection.owned(id).await?.ok_or(ReaderError::Protocol)?;
    if rospec_fingerprint(&readback)? != fingerprint || readback.get(9) != Some(&0) {
        return Err(ReaderError::Protocol);
    }
    store
        .tracking_reader_applied_clocked(reader, version, &fingerprint, || clock.now())
        .await
        .map_err(|error| ReaderError::State(error.code()))?;
    connection
        .request(24, 34, id.to_be_bytes().to_vec())
        .await?;
    Ok(fingerprint)
}
fn reader_event_time(message: &Message) -> Option<DateTime<Utc>> {
    if message.kind != 63 {
        return None;
    }
    parameters(&message.body)
        .ok()?
        .into_iter()
        .find(|p| p.kind == 246)
        .and_then(|data| parameters(data.body).ok())?
        .iter()
        .find(|p| p.kind == 128)
        .and_then(|time| timestamp(time.body).ok())
}

fn reader_connection_time(message: &Message) -> Option<DateTime<Utc>> {
    let outer = parameters(&message.body).ok()?;
    let data = outer.iter().find(|p| p.kind == 246)?;
    let fields = parameters(data.body).ok()?;
    fields
        .iter()
        .any(|p| p.kind == 256 && be16(p.body, 0).ok() == Some(0))
        .then(|| reader_event_time(message))
        .flatten()
}

/// TCP is the R700's documented LLRP transport, not authenticated HTTPS. Run
/// only on a deliberately isolated private reader network. The worker accepts
/// no arbitrary LLRP commands and cannot encode any tag-memory AccessSpec.
pub async fn run_llrp(
    store: &Store,
    reader: &ReaderConfig,
    ports: &[PortConfig],
    clock: &dyn ReaderClock,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ReaderError> {
    let ReaderConnection::ImpinjLlrp {
        host,
        port,
        owned_rospec_id,
    } = &reader.connection
    else {
        return Err(ReaderError::Configuration);
    };
    let initial = store
        .tracking_snapshot_clocked(|| clock.now())
        .await
        .map_err(|error| ReaderError::State(error.code()))?;
    let context = LlrpRun {
        store,
        reader,
        ports,
        owned_id: *owned_rospec_id,
        clock,
        configuration_version: initial.configuration_version,
    };
    let mut backoff = ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(30))?;
    let mut envelope_at = None;
    loop {
        let snapshot = store
            .tracking_snapshot_clocked(|| clock.now())
            .await
            .map_err(|error| ReaderError::State(error.code()))?;
        let current = context.matches(&snapshot);
        let disabled = !snapshot.config.reader_active(&reader.id);
        let claimed = snapshot
            .reader_operations
            .iter()
            .any(|op| op.reader_id == reader.id && op.requires_cleanup())
            || snapshot
                .reader_controls
                .iter()
                .any(|op| op.reader_id == reader.id && op.requires_cleanup());
        let recovery_only = disabled || !current || *shutdown.borrow();
        if recovery_only && !claimed {
            return Ok(());
        }
        let started = clock.elapsed();
        let connect_started_at = clock.now();
        // A pending hardware owner must make one bounded cleanup attempt even
        // when shutdown arrived before its task could begin.
        let connection = if claimed || recovery_only {
            Connection::connect(host, *port).await
        } else {
            tokio::select! {biased;_ = shutdown.changed()=>return Ok(()),result=Connection::connect(host,*port)=>result}
        };
        let mut decoder = CycleDecoder {
            reader: reader.id.clone(),
            owned: *owned_rospec_id,
            ports: Vec::new(),
            session: Uuid::now_v7(),
            sequence: 0,
            armed_after: connect_started_at,
            started: None,
            next_spec: 0,
            interval_start: None,
            connected: BTreeSet::new(),
            failed: false,
            ended: false,
            data_source: DataSource::Live,
            latest_reads: BTreeMap::new(),
            last_event_at: None,
            envelope_at,
            time_basis: ReaderTimeBasis::default(),
        };
        let mut fault = ReaderFault::Disconnected;
        let mut cleaned = false;
        if let Ok(mut connection) = connection {
            let mut operation_shutdown = shutdown.clone();
            let result = if recovery_only || *shutdown.borrow() {
                Ok(())
            } else {
                tokio::select! {biased;_ = shutdown.changed()=>Ok(()), result=operate(&context,&mut connection,&mut decoder,&mut operation_shutdown)=>result}
            };
            fault = match result {
                Err(
                    ReaderError::Unsupported
                    | ReaderError::Configuration
                    | ReaderError::RecoveryRequired,
                ) => ReaderFault::RecoveryRequired,
                Err(ReaderError::ClockSkew) => ReaderFault::ClockSkew,
                Err(ReaderError::Protocol | ReaderError::Oversized) => ReaderFault::Protocol,
                _ => ReaderFault::Disconnected,
            };
            let cleanup =
                tokio::time::timeout(IO_TIMEOUT, cleanup_reader(&context, &mut connection)).await;
            if let Ok(Err(ReaderError::State(code))) = cleanup {
                return Err(ReaderError::State(code));
            }
            cleaned = matches!(cleanup, Ok(Ok(())));
            if !cleaned {
                fault = ReaderFault::RecoveryRequired;
            }
            if matches!(result, Err(ReaderError::State(_))) {
                return result;
            }
            let latest = store
                .tracking_snapshot_clocked(|| clock.now())
                .await
                .map_err(|error| ReaderError::State(error.code()))?;
            if recovery_only || !context.matches(&latest) {
                // Intentional disable and retired generations emit no old
                // observations and can never provision the old ports again.
                return if cleaned {
                    Ok(())
                } else {
                    Err(ReaderError::RecoveryRequired)
                };
            }
        }
        let latest = store
            .tracking_snapshot_clocked(|| clock.now())
            .await
            .map_err(|error| ReaderError::State(error.code()))?;
        if !context.matches(&latest) {
            let pending = latest
                .reader_operations
                .iter()
                .any(|op| op.reader_id == reader.id && op.requires_cleanup())
                || latest
                    .reader_controls
                    .iter()
                    .any(|op| op.reader_id == reader.id && op.requires_cleanup());
            return if pending {
                Err(ReaderError::RecoveryRequired)
            } else {
                Ok(())
            };
        }
        if recovery_only {
            if *shutdown.borrow() || !current {
                return Err(ReaderError::RecoveryRequired);
            }
            // An unavailable disabled reader keeps its durable claims and retries
            // only cleanup, never setup or RF start.
        } else {
            let now = clock.now();
            let mut batch = decoder.batch(now)?;
            envelope_at = decoder.envelope_at;
            batch.connected = false;
            batch.coverage = ports
                .iter()
                .filter(|p| p.reader_id == reader.id && p.enabled)
                .map(|p| PortCoverage {
                    port_id: p.id.clone(),
                    started_at: now,
                    ended_at: now,
                    proof: CoverageProof::Fault { code: fault },
                })
                .collect();
            admit_disconnect(store, batch, clock).await?;
            if *shutdown.borrow() {
                return if cleaned {
                    Ok(())
                } else {
                    Err(ReaderError::RecoveryRequired)
                };
            }
        }
        if clock.elapsed().saturating_sub(started) > Duration::from_secs(90) {
            backoff.recovered();
        }
        tokio::select! {_ = shutdown.changed()=>{},_ = tokio::time::sleep(backoff.delay())=>{}}
    }
}

struct LlrpRun<'a> {
    store: &'a Store,
    reader: &'a ReaderConfig,
    ports: &'a [PortConfig],
    owned_id: u32,
    clock: &'a dyn ReaderClock,
    configuration_version: i64,
}
impl LlrpRun<'_> {
    fn matches(&self, snapshot: &TrackingSnapshot) -> bool {
        snapshot.configuration_version == self.configuration_version
            && snapshot
                .config
                .readers
                .iter()
                .any(|reader| reader == self.reader)
            && snapshot
                .config
                .ports
                .iter()
                .filter(|p| p.reader_id == self.reader.id)
                .eq(self.ports.iter().filter(|p| p.reader_id == self.reader.id))
    }
}
async fn cleanup_reader(
    context: &LlrpRun<'_>,
    connection: &mut Connection,
) -> Result<(), ReaderError> {
    let LlrpRun {
        store,
        reader,
        owned_id,
        clock,
        ..
    } = *context;
    let snapshot = store
        .tracking_snapshot_clocked(|| clock.now())
        .await
        .map_err(|error| ReaderError::State(error.code()))?;
    // Extensions are connection-scoped; fresh cleanup must read every exact
    // vendor parameter before comparing a durably claimed ROSpec fingerprint.
    connection.enable_extensions().await?;
    let claim = snapshot
        .reader_operations
        .iter()
        .find(|op| op.reader_id == reader.id);
    let observed = connection.owned_for_cleanup(owned_id).await?;
    let release = if let Some(owned) = observed {
        let observed = rospec_fingerprint(&owned)?;
        let claim = claim.ok_or(ReaderError::RecoveryRequired)?;
        if claim.owned_rospec_id != owned_id || !claim.known_fingerprints.contains(&observed) {
            return Err(ReaderError::RecoveryRequired);
        }
        stop_owned(connection, owned_id, &observed).await?;
        connection
            .request(21, 31, owned_id.to_be_bytes().to_vec())
            .await?;
        if connection.owned_for_cleanup(owned_id).await?.is_some() {
            return Err(ReaderError::Protocol);
        }
        Some(observed)
    } else {
        // A crash after DELETE but before its local acknowledgement is already
        // safe: actual absence releases the exact existing ownership claim.
        claim
            .filter(|op| op.requires_cleanup())
            .map(|claim| {
                if claim.owned_rospec_id != owned_id {
                    return Err(ReaderError::RecoveryRequired);
                }
                claim
                    .applied_fingerprint
                    .as_ref()
                    .or(claim.pending_fingerprint.as_ref())
                    .or(claim.known_fingerprints.last())
                    .cloned()
                    .ok_or(ReaderError::RecoveryRequired)
            })
            .transpose()?
    };
    if let Some(fingerprint) = release {
        store
            .tracking_reader_released_clocked(&reader.id, owned_id, &fingerprint, || clock.now())
            .await
            .map_err(|error| ReaderError::State(error.code()))?;
    }
    if snapshot
        .reader_controls
        .iter()
        .any(|op| op.reader_id == reader.id && op.requires_cleanup())
    {
        restore_control(connection, store, &reader.id, clock).await?;
    }
    connection.request(14, 4, Vec::new()).await?;
    Ok(())
}
async fn operate(
    context: &LlrpRun<'_>,
    connection: &mut Connection,
    decoder: &mut CycleDecoder,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), ReaderError> {
    let LlrpRun {
        store,
        reader,
        ports,
        owned_id,
        clock,
        configuration_version: version,
    } = *context;
    let mut fingerprint = None;
    let snapshot = store
        .tracking_snapshot_clocked(|| clock.now())
        .await
        .map_err(|error| ReaderError::State(error.code()))?;
    if !context.matches(&snapshot) {
        return Ok(());
    }
    // Inspect before any mutation. An unrelated ROSpec or ANY AccessSpec is a
    // commissioning conflict, never authority to disable another application.
    connection.owned(owned_id).await?;
    let access = connection.request(44, 54, Vec::new()).await?;
    if parameters(&access.body)?.iter().any(|p| p.kind == 207) {
        return Err(ReaderError::Unsupported);
    }
    connection.enable_extensions().await?; // connection-scoped only; no RF/write operation
    let caps = capabilities(&connection.request(1, 11, vec![0]).await?)?;
    if snapshot
        .reader_controls
        .iter()
        .any(|op| op.reader_id == reader.id && op.phase == ReaderControlPhase::RestorePending)
    {
        restore_control(connection, store, &reader.id, clock).await?;
    }
    let config = connection.config().await?;
    let expected_control = desired_control(&control_settings(&config)?);
    let (all_ports, connected) = scan_ports(&config, &caps, ports, &reader.id)?;
    decoder.connected = connected;
    let mut barrier = connection
        .pending
        .iter()
        .filter_map(reader_event_time)
        .max()
        .ok_or(ReaderError::Unsupported)?;
    if barrier > clock.now() + chrono::Duration::seconds(2)
        || clock.now() - barrier > chrono::Duration::seconds(60)
    {
        return Err(ReaderError::Protocol);
    }
    let connection_time = connection
        .pending
        .iter()
        .filter_map(reader_connection_time)
        .min()
        .ok_or(ReaderError::Unsupported)?;
    decoder.time_basis = ReaderTimeBasis::from_sample(connection_time, decoder.armed_after);
    let mut selected: Vec<PortId> = Vec::new();
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let snapshot = store
            .tracking_snapshot_clocked(|| clock.now())
            .await
            .map_err(|error| ReaderError::State(error.code()))?;
        if !context.matches(&snapshot) {
            return Ok(());
        }
        if !selected.is_empty()
            && !snapshot
                .reader_readbacks
                .iter()
                .any(|readback| readback.reader_id == reader.id)
        {
            return Err(ReaderError::EvidenceChanged);
        }
        let borrowed = snapshot.sessions.iter().find(|session| {
            all_ports.iter().any(|p| p.config.id == session.port_id)
                && matches!(
                    session.borrow_state,
                    BorrowState::PauseRequested | BorrowState::Paused
                )
        });
        let next: Vec<ScanPort> = if let Some(session) = borrowed {
            all_ports
                .iter()
                .filter(|p| p.config.id == session.port_id)
                .cloned()
                .collect()
        } else {
            all_ports.clone()
        };
        let ids: Vec<PortId> = next.iter().map(|p| p.config.id.clone()).collect();
        if ids != selected {
            let desired = rospec(owned_id, &next)?;
            fingerprint = Some(
                configure_owned(
                    connection, store, &reader.id, version, owned_id, &desired, clock,
                )
                .await?,
            );
            selected = ids;
            decoder.ports = next;
            decoder.session = Uuid::now_v7();
            decoder.sequence = 0;
            let mut batch = decoder.batch(clock.now())?;
            batch.readback = Some(ReaderReadback {
                reader_id: reader.id.clone(),
                at: clock.now(),
                capabilities: ReaderCapabilities {
                    antenna_ports: (1..=caps.max_antenna).collect(),
                    min_power_cdbm: caps.powers.values().copied().min(),
                    max_power_cdbm: caps.powers.values().copied().max(),
                    min_dwell_ms: Some(1),
                    max_dwell_ms: Some(60_000),
                    continuous_inventory_proof: true,
                    inventory_pause: true,
                    firmware: Some(caps.firmware.clone()),
                },
                ports: all_ports
                    .iter()
                    .map(|p| PortReadback {
                        port_id: p.config.id.clone(),
                        antenna: p.config.antenna,
                        enabled: selected.contains(&p.config.id),
                        transmit_power_cdbm: parameters(&p.transmitter)
                            .ok()
                            .and_then(|list| list.first().and_then(|p| be16(p.body, 4).ok()))
                            .and_then(|index| caps.powers.get(&index).copied()),
                        dwell_ms: Some(p.dwell),
                    })
                    .collect(),
            });
            admit_live(store, batch, clock).await?;
        }
        // START only after STOP/readback and old-report drain. Events generated
        // while disconnected are discarded by Octane (manual3.1.16), not replayed.
        connection.send(60, 0, Vec::new()).await?;
        let current = connection
            .owned(owned_id)
            .await?
            .ok_or(ReaderError::Protocol)?;
        if fingerprint.as_deref() != Some(&rospec_fingerprint(&current)?)
            || current.get(9) != Some(&1)
        {
            return Err(ReaderError::RecoveryRequired);
        }
        if connection.control_readback().await? != expected_control {
            return Err(ReaderError::RecoveryRequired);
        }
        if let Some(time) = connection
            .pending
            .iter()
            .filter_map(reader_event_time)
            .max()
        {
            barrier = barrier.max(time);
        }
        connection.pending.clear();
        decoder.arm(barrier);
        let current = store
            .tracking_snapshot_clocked(|| clock.now())
            .await
            .map_err(|error| ReaderError::State(error.code()))?;
        if !context.matches(&current) {
            return Ok(());
        }
        connection
            .request(22, 32, owned_id.to_be_bytes().to_vec())
            .await?;
        if let Some(session) = borrowed {
            acknowledge_borrow(store, reader, session.id, BorrowState::Paused, clock).await?;
        } else {
            for session in snapshot.sessions.iter().filter(|session| {
                all_ports.iter().any(|p| p.config.id == session.port_id)
                    && matches!(
                        session.borrow_state,
                        BorrowState::RestoreRequested | BorrowState::Failed
                    )
            }) {
                acknowledge_borrow(store, reader, session.id, BorrowState::Restored, clock).await?;
            }
        }
        let cycle_limit = Duration::from_millis(
            decoder
                .ports
                .iter()
                .map(|p| u64::from(p.dwell))
                .sum::<u64>(),
        )
        .saturating_add(IO_TIMEOUT);
        let began = clock.elapsed();
        while !decoder.ended {
            if clock.elapsed().saturating_sub(began) > cycle_limit {
                return Err(ReaderError::Transport);
            }
            let message = tokio::select! {biased;_ = shutdown.changed()=>return Ok(()),result=tokio::time::timeout(Duration::from_millis(250),connection.next())=>result};
            let message = match message {
                Ok(message) => message?,
                Err(_) => {
                    let current = store
                        .tracking_snapshot_clocked(|| clock.now())
                        .await
                        .map_err(|error| ReaderError::State(error.code()))?;
                    if !context.matches(&current) {
                        return Ok(());
                    }
                    continue;
                }
            };
            let batch = match message.kind {
                62 => {
                    connection.send(72, message.id, Vec::new()).await?;
                    None
                }
                63 => decoder.event(&message, clock.now())?,
                61 => decoder.reports(&message, clock.now())?,
                _ => return Err(ReaderError::Protocol),
            };
            if let Some(batch) = batch {
                route_batch(store, decoder, ports, batch, clock).await?;
            }
        }
        barrier = decoder.last_event_at.ok_or(ReaderError::Protocol)?;
    }
}

async fn route_batch(
    store: &Store,
    decoder: &mut CycleDecoder,
    ports: &[PortConfig],
    mut batch: ObservationBatch,
    clock: &dyn ReaderClock,
) -> Result<(), ReaderError> {
    let snapshot = store
        .tracking_snapshot_clocked(|| clock.now())
        .await
        .map_err(|error| ReaderError::State(error.code()))?;
    if !snapshot
        .reader_readbacks
        .iter()
        .any(|readback| readback.reader_id == batch.reader_id)
    {
        return Err(ReaderError::EvidenceChanged);
    }
    let mut inventory = batch.clone();
    inventory.observations.clear();
    let mut captures: BTreeMap<SessionId, Vec<TagObservation>> = BTreeMap::new();
    for observation in batch.observations.drain(..) {
        let port = ports
            .iter()
            .find(|p| p.id == observation.port_id)
            .ok_or(ReaderError::Configuration)?;
        if let Some(session) = snapshot
            .sessions
            .iter()
            .find(|s| s.port_id == port.id && s.state == EnrollmentState::Active)
        {
            if observation.first_seen_at > session.capture_started_at {
                captures.entry(session.id).or_default().push(observation);
            }
        } else if port.role == PortRole::Monitor {
            inventory.observations.push(observation);
        }
    }
    if !inventory.observations.is_empty() || !inventory.coverage.is_empty() {
        decoder.sequence = decoder
            .sequence
            .checked_add(1)
            .ok_or(ReaderError::Protocol)?;
        inventory.sequence = decoder.sequence;
        admit_live(store, inventory, clock).await?;
    }
    for (session, observations) in captures {
        decoder.sequence = decoder
            .sequence
            .checked_add(1)
            .ok_or(ReaderError::Protocol)?;
        let mut capture = batch.clone();
        capture.sequence = decoder.sequence;
        capture.purpose = ObservationPurpose::Enrollment;
        capture.capture_session = Some(session);
        capture.coverage.clear();
        capture.observations = observations;
        admit_live(store, capture, clock).await?;
    }
    Ok(())
}
async fn acknowledge_borrow(
    store: &Store,
    reader: &ReaderConfig,
    session: SessionId,
    state: BorrowState,
    clock: &dyn ReaderClock,
) -> Result<(), ReaderError> {
    let state_name = match state {
        BorrowState::Paused => "paused",
        BorrowState::Restored => "restored",
        _ => return Err(ReaderError::Configuration),
    };
    for _ in 0..3 {
        let snapshot = store
            .tracking_snapshot_clocked(|| clock.now())
            .await
            .map_err(|error| ReaderError::State(error.code()))?;
        let Some(current) = snapshot.sessions.iter().find(|s| s.id == session) else {
            return Ok(());
        };
        if current.borrow_state == state {
            return Ok(());
        }
        let intended = matches!(
            (current.borrow_state, state),
            (BorrowState::PauseRequested, BorrowState::Paused)
                | (
                    BorrowState::RestoreRequested | BorrowState::Failed,
                    BorrowState::Restored
                )
        );
        if !intended {
            return Ok(());
        }
        // Resume/reset creates a new capture intent. Reusing the first pause's
        // idempotency key would replay its old response without pausing again.
        let key = format!(
            "reader-borrow-{}",
            hex(&Sha256::digest(format!(
                "{}:{session}:{state_name}:{}",
                reader.id, current.capture_version
            )))
        );
        let result = store
            .tracking_command_clocked(
                &format!("reader:{}", reader.id),
                &key,
                snapshot.tracking_revision,
                TrackingCommand::AcknowledgeBorrow {
                    session_id: session,
                    state,
                },
                || clock.now(),
            )
            .await;
        match result {
            Ok(_) => return Ok(()),
            Err(TrackingError::Revision) => continue,
            Err(error) => return Err(ReaderError::State(error.code())),
        }
    }
    Err(ReaderError::EvidenceChanged)
}

#[cfg(test)]
mod tests {
    use crate::tracking::reader::llrp::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    fn at(millis: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_800_000_000_000 + millis).unwrap()
    }
    fn scan() -> ScanPort {
        ScanPort {
            config: PortConfig {
                id: "port-a".parse().unwrap(),
                reader_id: "reader-a".parse().unwrap(),
                antenna: 1,
                area_id: Some("area-a".parse().unwrap()),
                role: PortRole::Monitor,
                enabled: true,
                required_for_absence: true,
                commissioned: true,
                transmit_power_cdbm: None,
                dwell_ms: Some(250),
            },
            dwell: 250,
            transmitter: tlv(224, &[0, 1, 0, 1, 0, 1]).unwrap(),
        }
    }
    fn decoder() -> CycleDecoder {
        CycleDecoder {
            reader: "reader-a".parse().unwrap(),
            owned: 1001,
            ports: vec![scan()],
            session: Uuid::from_u128(1),
            sequence: 0,
            armed_after: at(0),
            started: None,
            next_spec: 0,
            interval_start: None,
            connected: BTreeSet::from([1]),
            failed: false,
            ended: false,
            data_source: DataSource::Simulation,
            latest_reads: BTreeMap::new(),
            last_event_at: None,
            envelope_at: None,
            time_basis: ReaderTimeBasis::default(),
        }
    }
    fn event(time: i64, kind: u16, body: Vec<u8>) -> Message {
        let mut data = tlv(
            128,
            &u64::try_from(at(time).timestamp_micros())
                .unwrap()
                .to_be_bytes(),
        )
        .unwrap();
        data.extend(tlv(kind, &body).unwrap());
        Message {
            kind: 63,
            id: 0,
            body: tlv(246, &data).unwrap(),
        }
    }
    fn ro_event(time: i64, kind: u8, id: u32) -> Message {
        let mut body = vec![kind];
        body.extend(id.to_be_bytes());
        body.extend(0u32.to_be_bytes());
        event(time, 249, body)
    }
    fn completion(time: i64, index: u16) -> Message {
        let mut body = vec![0];
        body.extend(1001u32.to_be_bytes());
        body.extend(index.to_be_bytes());
        event(time, 254, body)
    }
    fn tv(kind: u8, value: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x80 | kind];
        bytes.extend(value);
        bytes
    }
    fn report(first: i64, last: i64, epc: &[u8]) -> Message {
        let mut data = 2u16.to_be_bytes().to_vec();
        data.extend(epc);
        data[..2].copy_from_slice(&u16::try_from(epc.len() * 8).unwrap().to_be_bytes());
        let mut report = tlv(241, &data).unwrap();
        report.extend(tv(9, &1001u32.to_be_bytes()));
        report.extend(tv(14, &1u16.to_be_bytes()));
        report.extend(tv(10, &1u16.to_be_bytes()));
        report.extend(tv(1, &1u16.to_be_bytes()));
        report.extend(tv(
            2,
            &u64::try_from(at(first).timestamp_micros())
                .unwrap()
                .to_be_bytes(),
        ));
        report.extend(tv(
            4,
            &u64::try_from(at(last).timestamp_micros())
                .unwrap()
                .to_be_bytes(),
        ));
        report.extend(tv(8, &3u16.to_be_bytes()));
        report.extend(tv(6, &[199]));
        Message {
            kind: 61,
            id: 0,
            body: tlv(240, &report).unwrap(),
        }
    }
    #[test]
    fn empty_area_requires_fresh_start_connected_antenna_and_exact_completed_dwell() {
        let mut decoder = decoder();
        assert!(decoder.event(&completion(250, 1), at(250)).is_err());
        let mut decoder = super::tests::decoder();
        let start = decoder
            .event(&ro_event(100, 0, 1001), at(100))
            .unwrap()
            .unwrap();
        assert!(
            start
                .coverage
                .iter()
                .all(|p| p.proof == CoverageProof::TransportOnly)
        );
        let completed = decoder
            .event(&completion(350, 1), at(350))
            .unwrap()
            .unwrap();
        assert!(completed.observations.is_empty());
        assert_eq!(completed.coverage.len(), 1);
        assert_eq!(
            completed.coverage[0].proof,
            CoverageProof::VerifiedInventory
        );
        assert_eq!(
            (
                completed.coverage[0].started_at,
                completed.coverage[0].ended_at
            ),
            (at(100), at(350))
        );
        decoder.event(&ro_event(350, 1, 1001), at(350)).unwrap();
        assert!(decoder.ended);
        assert!(decoder.event(&completion(350, 1), at(350)).is_err());
    }
    #[test]
    fn missing_antenna_short_scan_wrong_spec_and_preemption_never_certify() {
        for bad in [
            completion(349, 1),
            completion(350, 2),
            ro_event(200, 2, 1001),
            ro_event(200, 0, 9999),
        ] {
            let mut decoder = decoder();
            decoder.event(&ro_event(100, 0, 1001), at(100)).unwrap();
            assert!(decoder.event(&bad, at(350)).is_err());
        }
        let mut decoder = decoder();
        decoder.connected.clear();
        decoder.event(&ro_event(100, 0, 1001), at(100)).unwrap();
        let batch = decoder
            .event(&completion(350, 1), at(350))
            .unwrap()
            .unwrap();
        assert_eq!(
            batch.coverage[0].proof,
            CoverageProof::Fault {
                code: ReaderFault::AntennaFault
            }
        );
    }
    #[test]
    fn old_buffered_reads_do_not_cross_rearm_barrier_and_real_counts_are_preserved() {
        let mut decoder = decoder();
        assert!(
            decoder
                .reports(&report(10, 20, &[0, 0, 1, 2]), at(100))
                .unwrap()
                .is_none()
        );
        decoder.event(&ro_event(100, 0, 1001), at(100)).unwrap();
        let old = decoder
            .reports(&report(10, 20, &[0, 0, 1, 2]), at(150))
            .unwrap()
            .unwrap();
        assert!(old.observations.is_empty());
        let batch = decoder
            .reports(&report(110, 140, &[0, 0, 1, 2]), at(150))
            .unwrap()
            .unwrap();
        assert_eq!(batch.observations[0].epc.as_str(), "00000102");
        assert_eq!(batch.observations[0].read_count, 3);
        assert_eq!(batch.observations[0].peak_rssi_cdbm, Some(-5700));
        decoder.arm(at(200));
        assert!(
            decoder
                .event(&ro_event(100, 0, 1001), at(200))
                .unwrap()
                .is_none()
        );
        assert!(
            decoder
                .reports(&report(110, 140, &[0, 0, 1, 2]), at(200))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn antenna_disconnect_exception_overflow_and_clock_skew_cannot_prove_coverage() {
        for bad in [
            event(200, 251, vec![]),
            event(200, 252, vec![0, 0]),
            event(200, 255, vec![0, 0, 1]),
        ] {
            let mut decoder = decoder();
            decoder.event(&ro_event(100, 0, 1001), at(100)).unwrap();
            let result = decoder.event(&bad, at(200));
            assert!(
                result.is_err()
                    || result
                        .unwrap()
                        .unwrap()
                        .coverage
                        .iter()
                        .all(|p| p.proof != CoverageProof::VerifiedInventory)
            );
            assert!(decoder.event(&completion(350, 1), at(350)).is_err());
        }
        assert!(
            decoder()
                .event(&ro_event(5_000, 0, 1001), at(100))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn rospec_scope_and_runtime_state_fingerprint_are_exact() {
        let bytes = rospec(1001, &[scan()]).unwrap();
        let fp = rospec_fingerprint(&bytes).unwrap();
        let mut active = bytes.clone();
        active[9] = 2;
        assert_eq!(rospec_fingerprint(&active).unwrap(), fp);
        let mut different = scan();
        different.dwell = 300;
        assert_ne!(
            rospec_fingerprint(&rospec(1001, &[different]).unwrap()).unwrap(),
            fp
        );
        let body = parameters(&bytes).unwrap()[0].body;
        let parts = parameters(&body[6..]).unwrap();
        let ai = parts.iter().find(|p| p.kind == 183).unwrap();
        assert_eq!(be16(ai.body, 0).unwrap(), 1);
        assert_eq!(be16(ai.body, 2).unwrap(), 1);
        let ai_parts = parameters(&ai.body[4..]).unwrap();
        let stop = ai_parts.iter().find(|p| p.kind == 184).unwrap();
        assert_eq!(stop.body, &[1, 0, 0, 0, 250]);
        let inventory = ai_parts.iter().find(|p| p.kind == 186).unwrap();
        let antenna = parameters(&inventory.body[3..]).unwrap();
        let command = parameters(&antenna[0].body[2..]).unwrap();
        let command = command.iter().find(|p| p.kind == 330).unwrap();
        let extensions = parameters(&command.body[1..]).unwrap();
        for (subtype, expected) in [
            (23, 6u16.to_be_bytes().to_vec()),
            (28, vec![0; 6]),
            (1554, vec![0]),
        ] {
            let found = extensions
                .iter()
                .find(|p| p.kind == 1023 && be32(p.body, 4).ok() == Some(subtype))
                .unwrap();
            assert_eq!(be32(found.body, 0).unwrap(), 25_882);
            assert_eq!(&found.body[8..], expected);
        }
    }
    #[test]
    fn fast_llrp_source_times_normalize_before_capture_and_manual_server_fences() {
        let mut decoder = decoder();
        decoder.time_basis = ReaderTimeBasis::from_sample(at(1900), at(0));
        decoder.arm(at(1900));
        decoder.event(&ro_event(2000, 0, 1001), at(100)).unwrap();
        let batch = decoder
            .reports(&report(2020, 2040, &[0, 171]), at(150))
            .unwrap()
            .unwrap();
        assert_eq!(batch.observations[0].first_seen_at, at(120));
        assert_eq!(batch.observations[0].last_seen_at, at(140));
        assert!(
            batch.observations[0].last_seen_at < at(160),
            "raw fast-reader UTC must not cross a server manual fence"
        );
        let completed = decoder
            .event(&completion(2250, 1), at(350))
            .unwrap()
            .unwrap();
        assert_eq!(completed.coverage[0].started_at, at(100));
        assert_eq!(completed.coverage[0].ended_at, at(350));
        assert!(matches!(
            completed.coverage[0].proof,
            CoverageProof::VerifiedInventory
        ));
    }
    #[test]
    fn rejected_clock_jump_does_not_poison_reconnect_envelopes_or_read_deduplication() {
        for report_jump in [false, true] {
            let mut decoder = decoder();
            decoder.time_basis = ReaderTimeBasis::from_sample(at(0), at(0));
            decoder.arm(at(0));
            decoder.event(&ro_event(100, 0, 1001), at(100)).unwrap();
            let result = if report_jump {
                decoder.reports(&report(120, 1950, &[0, 171]), at(150))
            } else {
                decoder.event(&completion(1950, 1), at(150))
            };
            assert_eq!(result.unwrap_err(), ReaderError::ClockSkew);
            assert_eq!(
                decoder.last_event_at,
                Some(at(100)),
                "rejected source clock must not become accepted ordering evidence"
            );
            assert!(decoder.latest_reads.is_empty());
            assert_eq!(decoder.batch(at(150)).unwrap().observed_at, at(150));
        }
    }
    #[test]
    fn binary_framing_bounds_and_invalid_parameters_are_rejected() {
        let message = completion(350, 1);
        let bytes = message.encode().unwrap();
        let mut framer = Framer::default();
        assert!(framer.push(&bytes[..9]).unwrap().is_empty());
        assert_eq!(framer.push(&bytes[9..]).unwrap(), [message]);
        assert!(framer.push(&[0x04, 0, 0, 16, 0, 0, 0, 0, 0, 0]).is_err());
        assert!(parameters(&[0x7f, 0xff, 0, 3]).is_err());
        assert!(parameters(&[0xff]).is_err());
    }
    #[test]
    fn manufacturer_core_capability_layout_uses_two_flag_bytes() {
        let mut general = 4u16.to_be_bytes().to_vec();
        general.extend([0x40, 0]);
        general.extend(25_882u32.to_be_bytes());
        general.extend(2_001_070u32.to_be_bytes());
        general.extend(6u16.to_be_bytes());
        general.extend(b"10.4.0");
        let mut body = tlv(137, &general).unwrap();
        let mut llrp = vec![0x48, 1, 0, 0];
        for limit in [1u32, 32, 1, 1, 1] {
            llrp.extend(limit.to_be_bytes());
        }
        body.extend(tlv(142, &llrp).unwrap());
        let mut reg = vec![0; 4];
        reg.extend(tlv(144, &tlv(145, &[0, 1, 11, 184]).unwrap()).unwrap());
        body.extend(tlv(143, &reg).unwrap());
        let caps = capabilities(&Message {
            kind: 11,
            id: 1,
            body,
        })
        .unwrap();
        assert_eq!(caps.firmware, "10.4.0");
        assert_eq!(caps.max_antenna, 4);
        assert_eq!(caps.powers.get(&1), Some(&3000));
    }
    #[tokio::test]
    async fn real_tcp_reader_handles_keepalive_partial_frames_and_ordered_notifications() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut header = [0u8; 10];
            stream.read_exact(&mut header).unwrap();
            assert_eq!(be16(&header, 0).unwrap() & 1023, 26);
            let keepalive = Message {
                kind: 62,
                id: 90,
                body: vec![],
            }
            .encode()
            .unwrap();
            stream.write_all(&keepalive[..3]).unwrap();
            stream.write_all(&keepalive[3..]).unwrap();
            stream.read_exact(&mut header).unwrap();
            assert_eq!(be16(&header, 0).unwrap() & 1023, 72);
            assert_eq!(be32(&header, 6).unwrap(), 90);
            stream
                .write_all(&ro_event(100, 0, 1001).encode().unwrap())
                .unwrap();
            let status = tlv(287, &[0, 0, 0, 0]).unwrap();
            stream
                .write_all(
                    &Message {
                        kind: 36,
                        id: 1,
                        body: status,
                    }
                    .encode()
                    .unwrap(),
                )
                .unwrap();
        });
        let mut connection = Connection::connect("127.0.0.1", port).await.unwrap();
        let response = connection.request(26, 36, vec![]).await.unwrap();
        assert_eq!(response.kind, 36);
        let notification = connection.next().await.unwrap();
        assert_eq!(notification.kind, 63);
        assert_eq!(reader_event_time(&notification), Some(at(100)));
        peer.join().unwrap();
    }

    fn status() -> Vec<u8> {
        tlv(287, &[0, 0, 0, 0]).unwrap()
    }
    fn fixture_capabilities() -> Vec<u8> {
        let mut general = 4u16.to_be_bytes().to_vec();
        general.extend([0x40, 0]);
        general.extend(25_882u32.to_be_bytes());
        general.extend(2_001_070u32.to_be_bytes());
        general.extend(6u16.to_be_bytes());
        general.extend(b"10.4.0");
        let mut result = status();
        result.extend(tlv(137, &general).unwrap());
        let mut llrp = vec![0x48, 1, 0, 0];
        for limit in [1u32, 32, 1, 1, 1] {
            llrp.extend(limit.to_be_bytes());
        }
        result.extend(tlv(142, &llrp).unwrap());
        let mut regulatory = vec![0; 4];
        regulatory.extend(tlv(144, &tlv(145, &[0, 1, 11, 184]).unwrap()).unwrap());
        result.extend(tlv(143, &regulatory).unwrap());
        result
    }
    struct PeerResult {
        mutations: Vec<u16>,
        original: LlrpControlSettings,
        final_settings: LlrpControlSettings,
        remaining_program: bool,
        final_program: Option<Vec<u8>>,
        connections: usize,
    }
    fn fixture_original_control() -> LlrpControlSettings {
        LlrpControlSettings {
            notifications: vec![
                LlrpEventSetting {
                    event_type: 2,
                    enabled: false,
                },
                LlrpEventSetting {
                    event_type: 4,
                    enabled: true,
                },
            ],
            keepalive: LlrpKeepalive {
                trigger: KeepaliveTrigger::None,
                interval_ms: 0,
            },
            hold_events_and_reports_upon_reconnect: true,
        }
    }
    struct PeerSeed {
        program: Option<Vec<u8>>,
        settings: LlrpControlSettings,
    }
    struct PeerScanControl {
        paused: watch::Sender<usize>,
        resume: watch::Receiver<usize>,
        fresh_tag: bool,
    }
    async fn fixture_reader(
        listener: tokio::net::TcpListener,
        first_clock_skew: chrono::Duration,
        expected_connections: usize,
        seed: Option<PeerSeed>,
        mut scans: Option<PeerScanControl>,
    ) -> PeerResult {
        let original = fixture_original_control();
        let mut settings = seed
            .as_ref()
            .map_or_else(|| original.clone(), |seed| seed.settings.clone());
        let mut program = seed.and_then(|seed| seed.program);
        let mut mutations = Vec::new();
        let mut connections = 0;
        'connections: loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = Connection {
                socket,
                framer: Framer::default(),
                pending: VecDeque::new(),
                id: 0,
            };
            connections += 1;
            let mut completed_scans = 0;
            let time = Utc::now()
                + if connections == 1 {
                    first_clock_skew
                } else {
                    chrono::Duration::zero()
                };
            let mut data = tlv(
                128,
                &u64::try_from(time.timestamp_micros())
                    .unwrap()
                    .to_be_bytes(),
            )
            .unwrap();
            data.extend(tlv(256, &0u16.to_be_bytes()).unwrap());
            connection
                .send(63, 0, tlv(246, &data).unwrap())
                .await
                .unwrap();
            if connections == 2 {
                let stale = (Utc::now() - chrono::Duration::seconds(10)).timestamp_millis()
                    - 1_800_000_000_000;
                let old = report(stale, stale, &[0, 0, 17, 34]);
                connection.send(old.kind, old.id, old.body).await.unwrap();
            }
            loop {
                let message = match tokio::time::timeout(Duration::from_secs(10), connection.next())
                    .await
                    .unwrap()
                {
                    Ok(message) => message,
                    Err(ReaderError::Transport) => break 'connections,
                    Err(error) => panic!("fictional reader failed: {error:?}"),
                };
                let (reply, body) = match message.kind {
                    1023 => {
                        assert_eq!(&message.body[..5], &[0, 0, 101, 26, 21]);
                        let mut response = vec![0, 0, 101, 26, 22];
                        response.extend(status());
                        (1023, response)
                    }
                    1 => (11, fixture_capabilities()),
                    2 => {
                        let mut body = status();
                        let requested = message.body[2];
                        let control = encode_control(&settings).unwrap();
                        for p in parameters(&control[1..]).unwrap() {
                            if requested == 0
                                || matches!((requested, p.kind), (5, 244) | (8, 220) | (11, 226))
                            {
                                body.extend(p.whole);
                            }
                        }
                        if requested == 0 {
                            body.extend(tlv(221, &[0x80, 0, 1, 0, 0]).unwrap());
                            let mut antenna = 1u16.to_be_bytes().to_vec();
                            antenna.extend(tlv(224, &[0, 1, 0, 1, 0, 1]).unwrap());
                            body.extend(tlv(222, &antenna).unwrap());
                        }
                        (12, body)
                    }
                    3 => {
                        mutations.push(3);
                        assert_eq!(message.body.first(), Some(&0));
                        settings = control_settings(&Message {
                            kind: 12,
                            id: 0,
                            body: message.body[1..].to_vec(),
                        })
                        .unwrap();
                        (13, status())
                    }
                    44 => (54, status()),
                    26 => {
                        let mut body = status();
                        if let Some(program) = &program {
                            body.extend(program);
                        }
                        (36, body)
                    }
                    20 => {
                        mutations.push(20);
                        assert!(program.is_none());
                        assert_eq!(parameters(&message.body).unwrap()[0].kind, 177);
                        program = Some(message.body);
                        (30, status())
                    }
                    24 => {
                        mutations.push(24);
                        program.as_mut().unwrap()[9] = 1;
                        (34, status())
                    }
                    25 => {
                        mutations.push(25);
                        program.as_mut().unwrap()[9] = 0;
                        (35, status())
                    }
                    21 => {
                        mutations.push(21);
                        assert_eq!(be32(&message.body, 0).unwrap(), 1001);
                        program = None;
                        (31, status())
                    }
                    23 => {
                        mutations.push(23);
                        program.as_mut().unwrap()[9] = 1;
                        (33, status())
                    }
                    60 => continue,
                    22 => {
                        mutations.push(22);
                        assert_eq!(program.as_ref().unwrap()[9], 1);
                        program.as_mut().unwrap()[9] = 2;
                        connection.send(32, message.id, status()).await.unwrap();
                        let offset = if connections == 1 {
                            first_clock_skew
                        } else {
                            chrono::Duration::zero()
                        };
                        let started = Utc::now() + offset;
                        let mut ro = vec![0];
                        ro.extend(1001u32.to_be_bytes());
                        ro.extend(0u32.to_be_bytes());
                        let mut data = tlv(
                            128,
                            &u64::try_from(started.timestamp_micros())
                                .unwrap()
                                .to_be_bytes(),
                        )
                        .unwrap();
                        data.extend(tlv(249, &ro).unwrap());
                        connection
                            .send(63, 0, tlv(246, &data).unwrap())
                            .await
                            .unwrap();
                        tokio::time::sleep(Duration::from_millis(255)).await;
                        let ended = Utc::now() + offset;
                        if connections >= 2
                            && let Some(control) = &mut scans
                        {
                            if control.fresh_tag {
                                let first = started.timestamp_millis() + 2 - 1_800_000_000_000;
                                let last = ended.timestamp_millis() - 1_800_000_000_000;
                                let tag = report(first, last, &[0, 0, 17, 34]);
                                connection.send(tag.kind, tag.id, tag.body).await.unwrap();
                            }
                            let after = if control.fresh_tag { 0 } else { 2 };
                            if completed_scans == after {
                                // Synchronize at a running scan, with no
                                // control reply in flight. Observation of
                                // healthy state alone is not that boundary.
                                control.paused.send(connections).unwrap();
                                while *control.resume.borrow() < connections {
                                    control.resume.changed().await.unwrap();
                                }
                            }
                        }
                        completed_scans += 1;
                        let mut ai = vec![0];
                        ai.extend(1001u32.to_be_bytes());
                        ai.extend(1u16.to_be_bytes());
                        let mut data = tlv(
                            128,
                            &u64::try_from(ended.timestamp_micros())
                                .unwrap()
                                .to_be_bytes(),
                        )
                        .unwrap();
                        data.extend(tlv(254, &ai).unwrap());
                        connection
                            .send(63, 0, tlv(246, &data).unwrap())
                            .await
                            .unwrap();
                        program.as_mut().unwrap()[9] = 1;
                        ro[0] = 1;
                        let mut data = tlv(
                            128,
                            &u64::try_from(ended.timestamp_micros())
                                .unwrap()
                                .to_be_bytes(),
                        )
                        .unwrap();
                        data.extend(tlv(249, &ro).unwrap());
                        connection
                            .send(63, 0, tlv(246, &data).unwrap())
                            .await
                            .unwrap();
                        if connections == 1 {
                            break;
                        }
                        continue;
                    }
                    14 => {
                        connection.send(4, message.id, status()).await.unwrap();
                        if connections == expected_connections {
                            break 'connections;
                        }
                        break;
                    }
                    other => panic!("unexpected LLRP command {other}"),
                };
                connection.send(reply, message.id, body).await.unwrap();
            }
        }
        PeerResult {
            mutations,
            original,
            final_settings: settings,
            remaining_program: program.is_some(),
            final_program: program,
            connections,
        }
    }
    async fn worker_lifecycle(
        first_clock_skew: chrono::Duration,
        catalog_sync: bool,
        configuration_change: bool,
        enrollment_only: bool,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let expected_connections = if catalog_sync { 3 } else { 2 };
        let (finish_scan, scan_finished) = watch::channel(0_usize);
        let (pause_scan, mut scan_paused) = watch::channel(0_usize);
        let peer = tokio::spawn(fixture_reader(
            listener,
            first_clock_skew,
            expected_connections,
            None,
            Some(PeerScanControl {
                paused: pause_scan,
                resume: scan_finished,
                fresh_tag: enrollment_only,
            }),
        ));
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("fixture.sqlite3"))
            .await
            .unwrap();
        let mut catalog: Vec<crate::models::CatalogItem> =
            serde_json::from_str(include_str!("../../config/catalog.example.json")).unwrap();
        catalog.truncate(1);
        if catalog_sync {
            store
                .sync(
                    crate::models::SyncRequest {
                        items: catalog.clone(),
                        dry_run: false,
                        allow_empty: false,
                    },
                    Some(0),
                    "fixture",
                    Some("seed"),
                )
                .await
                .unwrap();
        }
        let reader = ReaderConfig {
            id: "reader-a".parse().unwrap(),
            label: "Fictional R700".into(),
            enabled: true,
            connection: ReaderConnection::ImpinjLlrp {
                host: "127.0.0.1".into(),
                port,
                owned_rospec_id: 1001,
            },
        };
        let mut port = scan().config;
        port.dwell_ms = None;
        if enrollment_only {
            port.role = PortRole::Enrollment;
            port.area_id = None;
            port.required_for_absence = false;
            port.commissioned = false;
        }
        let mut config = TrackingConfig {
            previous_wire: Default::default(),
            schema_version: TRACKING_SCHEMA_VERSION,
            areas: vec![AreaConfig {
                id: "area-a".parse().unwrap(),
                label: "Fictional area".into(),
                kind: AreaKind::Closet,
                enabled: true,
                thresholds: AreaThresholds::default(),
            }],
            readers: vec![reader.clone()],
            ports: vec![port.clone()],
            thresholds: Thresholds {
                warmup_ms: 300,
                ..Default::default()
            },
        };
        if enrollment_only {
            config.areas.clear();
        }
        let before = store.tracking_snapshot(Utc::now()).await.unwrap();
        store
            .tracking_command(
                "fixture",
                "configure",
                before.tracking_revision,
                TrackingCommand::Configure { config },
                Utc::now(),
            )
            .await
            .unwrap();
        if enrollment_only {
            let before = store.tracking_snapshot(Utc::now()).await.unwrap();
            assert!(!before.config.enabled());
            assert!(before.config.reader_active(&reader.id));
            store
                .tracking_command(
                    "fixture",
                    "lookup-before-area",
                    before.tracking_revision,
                    TrackingCommand::StartEnrollment {
                        unit_ids: Vec::new(),
                        port_id: port.id.clone(),
                        borrow: false,
                        purpose: EnrollmentPurpose::Lookup,
                        ttl_seconds: 300,
                    },
                    Utc::now(),
                )
                .await
                .unwrap();
        }
        let (stop, shutdown) = watch::channel(false);
        let worker_store = store.clone();
        let task = tokio::spawn(async move {
            let clock = crate::tracking::reader::SystemReaderClock::default();
            run_llrp(&worker_store, &reader, &[port], &clock, shutdown).await
        });
        let mut healthy = false;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let snapshot = store.tracking_snapshot(Utc::now()).await.unwrap();
            let enrollment_candidate = enrollment_only
                && snapshot.sessions.iter().any(|session| {
                    session
                        .candidates
                        .iter()
                        .any(|candidate| candidate.epc.as_str() == "00001122")
                });
            if snapshot.health.absence_coverage_healthy || enrollment_candidate {
                healthy = true;
                break;
            }
            if task.is_finished() {
                break;
            }
        }
        if healthy {
            tokio::time::timeout(
                Duration::from_secs(20),
                scan_paused.wait_for(|connection| *connection >= 2),
            )
            .await
            .unwrap()
            .unwrap();
        }
        if catalog_sync && healthy {
            healthy = false;
            catalog[0].quantity = Some(2);
            store
                .sync(
                    crate::models::SyncRequest {
                        items: catalog,
                        dry_run: false,
                        allow_empty: false,
                    },
                    Some(1),
                    "fixture",
                    Some("quantity-update"),
                )
                .await
                .unwrap();
            finish_scan.send(2).unwrap();
            for _ in 0..200 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let snapshot = store.tracking_snapshot(Utc::now()).await.unwrap();
                if snapshot.health.absence_coverage_healthy {
                    healthy = true;
                    break;
                }
                if task.is_finished() {
                    break;
                }
            }
        }
        if catalog_sync && healthy {
            tokio::time::timeout(
                Duration::from_secs(20),
                scan_paused.wait_for(|connection| *connection >= 3),
            )
            .await
            .unwrap()
            .unwrap();
        }
        let mut generation_exited = !configuration_change;
        if configuration_change && healthy {
            let snapshot = store.tracking_snapshot_clocked(Utc::now).await.unwrap();
            let mut config = snapshot.config;
            config.ports[0].dwell_ms = Some(300);
            store
                .tracking_command_clocked(
                    "fixture",
                    "new-generation",
                    snapshot.tracking_revision,
                    TrackingCommand::Configure { config },
                    Utc::now,
                )
                .await
                .unwrap();
            finish_scan.send(2).unwrap();
            for _ in 0..200 {
                if task.is_finished() {
                    generation_exited = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let _ = stop.send(true);
        let _ = finish_scan.send(expected_connections);
        let result = tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .unwrap()
            .unwrap();
        if result.is_err() {
            peer.abort();
            panic!("worker returned {result:?}");
        }
        if !healthy {
            peer.abort();
            panic!(
                "actual worker never recovered expected evidence: {:?}",
                store.tracking_snapshot(Utc::now()).await.unwrap()
            );
        }
        let peer = tokio::time::timeout(Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert!(
            healthy,
            "actual worker never earned completed empty coverage"
        );
        assert!(!peer.remaining_program);
        assert_eq!(peer.connections, expected_connections);
        assert!(
            generation_exited,
            "old worker kept reconnecting after its configuration generation changed"
        );
        assert_eq!(peer.original, peer.final_settings);
        assert!(peer.mutations.contains(&20));
        assert!(peer.mutations.contains(&21));
        assert!(
            peer.mutations
                .iter()
                .all(|kind| matches!(kind, 3 | 20 | 21 | 22 | 23 | 24 | 25))
        );
        let after = store.tracking_snapshot(Utc::now()).await.unwrap();
        assert!(!after.health.absence_coverage_healthy);
        if enrollment_only {
            assert!(!after.config.enabled());
            assert!(
                after
                    .diagnostics
                    .recent_reads
                    .iter()
                    .any(|read| read.epc.as_str() == "00001122"
                        && read.purpose == ObservationPurpose::Enrollment)
            );
            assert!(after.units.is_empty());
        } else {
            assert!(
                after.diagnostics.recent_reads.is_empty(),
                "buffered pre-reconnect tag leaked into fresh observations"
            );
        }
        assert!(
            after
                .reader_operations
                .iter()
                .all(|op| op.pending_fingerprint.is_none() && op.applied_fingerprint.is_none())
        );
        assert!(
            after
                .reader_controls
                .iter()
                .all(|op| op.phase == ReaderControlPhase::Restored)
        );
    }
    #[tokio::test]
    async fn real_worker_claims_program_proves_empty_scans_and_cleans_up_actual_sqlite_state() {
        worker_lifecycle(chrono::Duration::zero(), false, false, false).await;
    }
    #[tokio::test]
    async fn real_worker_reconnects_after_accepted_future_reader_timestamps() {
        worker_lifecycle(chrono::Duration::milliseconds(1900), false, false, false).await;
    }
    #[tokio::test]
    async fn real_worker_reinspects_and_resumes_after_normal_catalog_sync() {
        worker_lifecycle(chrono::Duration::zero(), true, false, false).await;
    }
    #[tokio::test]
    async fn real_worker_cleans_up_and_exits_its_old_configuration_generation() {
        worker_lifecycle(chrono::Duration::zero(), false, true, false).await;
    }
    #[tokio::test]
    async fn dedicated_enrollment_reads_factory_tag_before_any_area_exists() {
        worker_lifecycle(chrono::Duration::zero(), false, false, true).await;
    }
    #[tokio::test]
    async fn disabled_startup_restores_durable_claims_without_provisioning_or_observations() {
        #[derive(Clone, Copy)]
        enum Program {
            Owned,
            Absent,
            Unrelated,
            UnknownOwned,
        }
        for (reader_enabled, area_enabled, mode) in [
            (false, true, Program::Owned),
            (true, false, Program::Owned),
            (false, true, Program::Absent),
            (false, true, Program::Unrelated),
            (false, true, Program::UnknownOwned),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let reader = ReaderConfig {
                id: "reader-a".parse().unwrap(),
                label: "Fictional crashed reader".into(),
                enabled: true,
                connection: ReaderConnection::ImpinjLlrp {
                    host: "127.0.0.1".into(),
                    port,
                    owned_rospec_id: 1001,
                },
            };
            let mut scan = scan();
            scan.config.dwell_ms = None;
            let config = TrackingConfig {
                schema_version: TRACKING_SCHEMA_VERSION,
                areas: vec![AreaConfig {
                    id: "area-a".parse().unwrap(),
                    label: "Fictional area".into(),
                    kind: AreaKind::Closet,
                    enabled: true,
                    thresholds: AreaThresholds::default(),
                }],
                readers: vec![reader.clone()],
                ports: vec![scan.config.clone()],
                thresholds: Thresholds::default(),
                ..Default::default()
            };
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("crash.sqlite3");
            let store = Store::open(&path).await.unwrap();
            let snapshot = store.tracking_snapshot_clocked(Utc::now).await.unwrap();
            store
                .tracking_command_clocked(
                    "fixture",
                    "configure",
                    snapshot.tracking_revision,
                    TrackingCommand::Configure {
                        config: config.clone(),
                    },
                    Utc::now,
                )
                .await
                .unwrap();
            let version = store
                .tracking_snapshot_clocked(Utc::now)
                .await
                .unwrap()
                .configuration_version;
            let mut program = rospec(1001, std::slice::from_ref(&scan)).unwrap();
            let fingerprint = rospec_fingerprint(&program).unwrap();
            store
                .tracking_reader_plan_clocked(&reader.id, version, &fingerprint, None, Utc::now)
                .await
                .unwrap();
            store
                .tracking_reader_applied_clocked(&reader.id, version, &fingerprint, Utc::now)
                .await
                .unwrap();
            let original = fixture_original_control();
            let applied = desired_control(&original);
            store
                .tracking_reader_plan_control_clocked(
                    &reader.id,
                    version,
                    &original,
                    &applied,
                    Utc::now,
                )
                .await
                .unwrap();
            store
                .tracking_reader_control_applied_clocked(
                    &reader.id,
                    version,
                    &applied,
                    false,
                    Utc::now,
                )
                .await
                .unwrap();
            let mut disabled = config;
            disabled.areas[0].enabled = area_enabled;
            disabled.ports[0].enabled = area_enabled;
            disabled.readers[0].enabled = reader_enabled;
            let snapshot = store.tracking_snapshot_clocked(Utc::now).await.unwrap();
            store
                .tracking_command_clocked(
                    "fixture",
                    "disable",
                    snapshot.tracking_revision,
                    TrackingCommand::Configure {
                        config: disabled.clone(),
                    },
                    Utc::now,
                )
                .await
                .unwrap();
            store.close().await;
            // A real SQLite restart keeps only durable ownership/restore claims.
            let store = Store::open(&path).await.unwrap();
            program[9] = 2;
            match mode {
                Program::Unrelated => program[4..8].copy_from_slice(&1002u32.to_be_bytes()),
                Program::UnknownOwned => program[8] = 1,
                _ => {}
            }
            let actual_program = (!matches!(mode, Program::Absent)).then_some(program);
            let expected_program = match mode {
                Program::Unrelated | Program::UnknownOwned => actual_program.clone(),
                _ => None,
            };
            let expected_settings = if matches!(mode, Program::UnknownOwned) {
                applied.clone()
            } else {
                original.clone()
            };
            let peer = tokio::spawn(fixture_reader(
                listener,
                chrono::Duration::zero(),
                1,
                Some(PeerSeed {
                    program: actual_program,
                    settings: applied,
                }),
                None,
            ));
            let (_stop, shutdown) = watch::channel(false);
            let clock = crate::tracking::reader::SystemReaderClock::default();
            let result = tokio::time::timeout(
                Duration::from_secs(10),
                run_llrp(
                    &store,
                    &disabled.readers[0],
                    &disabled.ports,
                    &clock,
                    shutdown,
                ),
            )
            .await
            .unwrap();
            if matches!(mode, Program::UnknownOwned) {
                assert_eq!(result, Err(ReaderError::RecoveryRequired));
            } else if result.is_err() {
                peer.abort();
                panic!("disabled recovery returned {result:?}");
            }
            let peer = tokio::time::timeout(Duration::from_secs(5), peer)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                peer.final_program, expected_program,
                "unrelated/unclaimed programs must remain byte-exact"
            );
            assert_eq!(peer.final_settings, expected_settings);
            assert!(
                !peer
                    .mutations
                    .iter()
                    .any(|kind| matches!(kind, 20 | 22 | 24)),
                "disabled recovery provisioned or started RF"
            );
            assert_eq!(peer.mutations.contains(&21), matches!(mode, Program::Owned));
            if matches!(mode, Program::UnknownOwned) {
                assert!(
                    peer.mutations.is_empty(),
                    "unknown same-ID configuration was mutated"
                );
            }
            let snapshot = store.tracking_snapshot_clocked(Utc::now).await.unwrap();
            assert!(
                snapshot
                    .reader_operations
                    .iter()
                    .all(|op| op.requires_cleanup() == matches!(mode, Program::UnknownOwned))
            );
            assert!(
                snapshot
                    .reader_controls
                    .iter()
                    .all(|op| op.requires_cleanup() == matches!(mode, Program::UnknownOwned))
            );
            assert!(snapshot.reader_readbacks.is_empty());
            assert!(snapshot.diagnostics.faults.is_empty());
            assert!(snapshot.diagnostics.recent_reads.is_empty());
            store.close().await;
        }
    }
}
