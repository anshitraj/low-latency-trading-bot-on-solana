//! trader-shred-observer — READ-ONLY copy of the shreds this node receives.
//!
//! Runs next to agave-validator on our own RPC node. An AF_PACKET socket gets a
//! copy of every *incoming* UDP datagram (the validator still receives and
//! processes them normally; nothing is consumed, modified or sent to the
//! cluster). Data shreds are grouped per slot; as soon as an entry batch is
//! complete (its first and last data shreds are present and contiguous) it is
//! deshredded with Agave's own `Shredder::deshred`, decoded as an Alpenglow
//! `BlockComponent`, and the first signature (transaction id) of every
//! transaction in it is sent over UDP to the collectors (`--emit`).
//!
//! This is the earliest point our machine can know a transaction is in a
//! block: before replay, before any RPC notification.
//!
//! Missing data shreds are rebuilt from coding shreds with Agave's own Merkle
//! erasure recovery (`merkle::recover`, exposed by a one-function local build
//! patch, see agave-ledger-recover.patch.rs) once a 32+32 FEC set has >= 32
//! shreds. Limits (measurement only):
//! - Shred signatures are NOT verified against the slot leader. Fine for timing
//!   measurements; do not drive trading decisions from this feed until it is.
//!
//! Watch list (`--watch-bind 0.0.0.0:9901`): a collector may send 64-byte
//! signatures it is about to submit; every data shred (received or recovered)
//! is scanned for them and a hit is reported at once, without waiting for the
//! rest of the entry batch. The batch path still reports everything else.
//!
//! Wire format to collectors: datagrams of N x 72-byte records
//! [64-byte signature][u64 LE slot].
//!
//! Usage (root or CAP_NET_RAW):
//!   trader-shred-observer --emit 10.30.0.2:9900 [--emit 127.0.0.1:9901]
//!       [--ports 8000-8030] [--exclude 8001,8002,8003,8005]
//!       [--shred-version 50093]   (cluster shred version; strongly recommended)
//!       [--sink 20000]            (open + drain a UDP port for an external shred
//!                                  feed, so the kernel does not answer with ICMP
//!                                  port-unreachable; packets are still copied above)

use solana_entry::block_component::BlockComponent;
use solana_ledger::shred::{
    layout, recover_merkle_for_observer, ReedSolomonCache, Shred, ShredFlags, ShredType,
    Shredder, DATA_SHREDS_PER_FEC_BLOCK,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Watched signatures (first 8 bytes as u64 for a fast prefix scan).
type WatchList = Arc<RwLock<HashMap<[u8; 64], Instant>>>;
const WATCH_KEEP: Duration = Duration::from_secs(120);
/// Data shred headers precede the entry bytes.
const DATA_OFFSET: usize = 88;

/// Merkle shred payload sizes (solana-ledger 4.4.1): data 1203, code 1228
/// (= PACKET_DATA_SIZE 1232 - 4); repair responses append a 4-byte nonce.
const SHRED_PACKET_SIZES: [usize; 4] = [1203, 1207, 1228, 1232];
/// Shred version lives at bytes 77..79 of the common header.
const VERSION_OFFSET: usize = 77;
/// A real slot never jumps this far ahead of the newest one seen.
const MAX_SLOT_JUMP: u64 = 10_000;
const KEEP_SLOTS: u64 = 64;
const DEDUP_CAP: usize = 2_000_000;
const RECORD: usize = 72;
const MAX_RECORDS_PER_DATAGRAM: usize = 19; // 1368 bytes

#[derive(Default)]
struct SlotBuf {
    /// index -> (shred bytes, DATA_COMPLETE flag)
    data: BTreeMap<u32, (Vec<u8>, bool)>,
    /// batch end indices already decoded
    decoded_ends: HashSet<u32>,
}

/// Shreds of one erasure (FEC) set, for recovery.
#[derive(Default)]
struct FecSet {
    shreds: Vec<Shred>,
    ids: HashSet<(bool, u32)>,
    data_n: usize,
    recovered: bool,
}

#[derive(Default)]
struct Stats {
    packets: u64,
    udp_in_range: u64,
    data_shreds: u64,
    code_shreds: u64,
    dup_shreds: u64,
    batches: u64,
    watch_hits: u64,
    recovered_sets: u64,
    recovered_data: u64,
    recover_err: u64,
    bad_version: u64,
    bad_slot: u64,
    deshred_err: u64,
    decode_err: u64,
    markers: u64,
    txs: u64,
    sigs_emitted: u64,
    send_err: u64,
}

struct Observer {
    slots: HashMap<u64, SlotBuf>,
    fec: HashMap<(u64, u32), FecSet>,
    rs: ReedSolomonCache,
    max_slot: u64,
    seen: HashSet<[u8; 64]>,
    seen_order: VecDeque<[u8; 64]>,
    out: UdpSocket,
    emit: Vec<SocketAddr>,
    watch: Option<WatchList>,
    st: Stats,
}

impl Observer {
    /// Any shred (data or code): keep it in its FEC set; recover missing data
    /// shreds once enough of the set has arrived.
    fn on_shred(&mut self, bytes: &[u8], is_data: bool) {
        if is_data {
            self.on_data_shred(bytes);
        }
        let (Some(slot), Some(index), Some(fec_index)) = (
            layout::get_slot(bytes),
            layout::get_index(bytes),
            layout::get_fec_set_index(bytes),
        ) else {
            return;
        };
        if slot + KEEP_SLOTS < self.max_slot || slot > self.max_slot + MAX_SLOT_JUMP {
            return;
        }
        let set = self.fec.entry((slot, fec_index)).or_default();
        if set.recovered || !set.ids.insert((is_data, index)) {
            return;
        }
        let Ok(shred) = Shred::new_from_serialized_shred(bytes.to_vec()) else {
            return;
        };
        set.shreds.push(shred);
        if is_data {
            set.data_n += 1;
        }
        if set.data_n >= DATA_SHREDS_PER_FEC_BLOCK || set.shreds.len() < DATA_SHREDS_PER_FEC_BLOCK {
            return; // nothing missing, or not enough to recover yet
        }
        let shreds = set.shreds.clone();
        match recover_merkle_for_observer(shreds, &self.rs) {
            Ok(rec) => {
                set.recovered = true;
                self.st.recovered_sets += 1;
                let data: Vec<Vec<u8>> = rec
                    .into_iter()
                    .filter(|s| s.is_data())
                    .map(|s| s.payload().as_ref().to_vec())
                    .collect();
                for d in data {
                    self.st.recovered_data += 1;
                    self.on_data_shred(&d);
                }
            }
            Err(_) => self.st.recover_err += 1,
        }
    }

    fn on_data_shred(&mut self, bytes: &[u8]) {
        let (Some(slot), Some(index), Ok(flags)) = (
            layout::get_slot(bytes),
            layout::get_index(bytes),
            layout::get_flags(bytes),
        ) else {
            return;
        };
        if self.max_slot > 0 && slot > self.max_slot + MAX_SLOT_JUMP {
            self.st.bad_slot += 1;
            return; // implausible: not a shred of this cluster
        }
        if slot + KEEP_SLOTS < self.max_slot {
            return; // too old to matter
        }
        if slot > self.max_slot {
            self.max_slot = slot;
            let floor = slot.saturating_sub(KEEP_SLOTS);
            self.slots.retain(|s, _| *s >= floor);
            self.fec.retain(|(s, _), _| *s >= floor);
        }
        let complete = flags.contains(ShredFlags::DATA_COMPLETE_SHRED);
        let buf = self.slots.entry(slot).or_default();
        if buf.data.contains_key(&index) {
            self.st.dup_shreds += 1;
            return;
        }
        buf.data.insert(index, (bytes.to_vec(), complete));
        self.st.data_shreds += 1;
        self.check_watch(slot, bytes);
        self.try_batch(slot, index);
        // A DATA_COMPLETE shred also proves where the NEXT batch starts.
        if complete {
            self.try_batch(slot, index + 1);
        }
    }

    /// Decode the batch containing `index` if its start and end are known
    /// and every data shred in between is present.
    fn try_batch(&mut self, slot: u64, index: u32) {
        let Some(buf) = self.slots.get_mut(&slot) else {
            return;
        };
        if !buf.data.contains_key(&index) {
            return;
        }
        // End: walk forward through contiguous shreds to a DATA_COMPLETE one.
        let mut end = index;
        loop {
            match buf.data.get(&end) {
                Some((_, true)) => break,
                Some((_, false)) => end += 1,
                None => return, // gap before the end: wait
            }
        }
        if buf.decoded_ends.contains(&end) {
            return;
        }
        // Start: index 0, or right after a present DATA_COMPLETE shred.
        let mut start = index;
        loop {
            if start == 0 {
                break;
            }
            match buf.data.get(&(start - 1)) {
                Some((_, true)) => break,
                Some((_, false)) => start -= 1,
                None => return, // cannot prove where this batch starts yet
            }
        }
        let parts: Vec<&[u8]> = (start..=end)
            .map(|i| buf.data[&i].0.as_slice())
            .collect();
        buf.decoded_ends.insert(end);
        self.st.batches += 1;
        let bytes = match Shredder::deshred(parts) {
            Ok(b) => b,
            Err(_) => {
                self.st.deshred_err += 1;
                return;
            }
        };
        let component: BlockComponent = match wincode::deserialize(&bytes) {
            Ok(c) => c,
            Err(_) => {
                self.st.decode_err += 1;
                return;
            }
        };
        let BlockComponent::EntryBatch(entries) = component else {
            self.st.markers += 1;
            return;
        };
        let mut recs: Vec<u8> = Vec::with_capacity(RECORD * MAX_RECORDS_PER_DATAGRAM);
        let mut n = 0usize;
        for e in &entries {
            for tx in &e.transactions {
                self.st.txs += 1;
                let Some(sig) = tx.signatures.first() else {
                    continue;
                };
                let mut k = [0u8; 64];
                k.copy_from_slice(sig.as_ref());
                if !self.seen.insert(k) {
                    continue;
                }
                self.seen_order.push_back(k);
                if self.seen_order.len() > DEDUP_CAP {
                    if let Some(old) = self.seen_order.pop_front() {
                        self.seen.remove(&old);
                    }
                }
                recs.extend_from_slice(&k);
                recs.extend_from_slice(&slot.to_le_bytes());
                n += 1;
                if n == MAX_RECORDS_PER_DATAGRAM {
                    self.flush(&recs, n);
                    recs.clear();
                    n = 0;
                }
            }
        }
        if n > 0 {
            self.flush(&recs, n);
        }
    }

    /// Report watched signatures found anywhere in this data shred's bytes.
    fn check_watch(&mut self, slot: u64, bytes: &[u8]) {
        let Some(w) = &self.watch else {
            return;
        };
        let hits: Vec<[u8; 64]> = {
            let g = w.read().unwrap();
            if g.is_empty() || bytes.len() < DATA_OFFSET + 64 {
                return;
            }
            let data = &bytes[DATA_OFFSET..];
            g.keys()
                .filter(|sig| {
                    let p = u64::from_le_bytes(sig[..8].try_into().unwrap());
                    (0..=data.len() - 64).any(|i| {
                        u64::from_le_bytes(data[i..i + 8].try_into().unwrap()) == p
                            && data[i..i + 64] == sig[..]
                    })
                })
                .copied()
                .collect()
        };
        for sig in hits {
            if !self.seen.insert(sig) {
                continue;
            }
            self.seen_order.push_back(sig);
            self.st.watch_hits += 1;
            let mut rec = sig.to_vec();
            rec.extend_from_slice(&slot.to_le_bytes());
            self.flush(&rec, 1);
        }
    }

    fn flush(&mut self, recs: &[u8], n: usize) {
        for a in &self.emit {
            if self.out.send_to(recs, a).is_err() {
                self.st.send_err += 1;
            }
        }
        self.st.sigs_emitted += n as u64;
    }
}

fn parse_ports(s: &str) -> Vec<u16> {
    let mut v = vec![];
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u16, u16) = (a.parse().expect("port"), b.parse().expect("port"));
                v.extend(a..=b);
            }
            None => v.push(part.parse().expect("port")),
        }
    }
    v
}

fn main() {
    let mut emit: Vec<SocketAddr> = vec![];
    let mut ports = parse_ports("8000-8030");
    let mut exclude: Vec<u16> = vec![];
    let mut shred_version: Option<u16> = None;
    let mut watch_bind: Option<String> = None;
    let mut sinks: Vec<u16> = vec![];
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--emit" => emit.push(args.next().expect("--emit addr").parse().expect("addr")),
            "--ports" => ports = parse_ports(&args.next().expect("--ports")),
            "--exclude" => exclude = parse_ports(&args.next().expect("--exclude")),
            "--watch-bind" => watch_bind = Some(args.next().expect("--watch-bind addr")),
            "--sink" => sinks.extend(parse_ports(&args.next().expect("--sink ports"))),
            "--shred-version" => {
                shred_version = Some(args.next().expect("--shred-version").parse().expect("u16"))
            }
            other => panic!("unknown arg {other}"),
        }
    }
    assert!(!emit.is_empty(), "need at least one --emit addr");
    let mut port_ok = [false; 65536];
    for p in &ports {
        port_ok[*p as usize] = true;
    }
    for p in &exclude {
        port_ok[*p as usize] = false;
    }

    // AF_PACKET / SOCK_DGRAM / ETH_P_IP: a copy of each IPv4 packet, starting
    // at the IP header. Read-only.
    let proto = (libc::ETH_P_IP as u16).to_be() as i32;
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM, proto) };
    assert!(fd >= 0, "AF_PACKET socket failed (need root or CAP_NET_RAW): {}", std::io::Error::last_os_error());
    let rcvbuf: libc::c_int = 256 << 20;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &rcvbuf as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    for p in &sinks {
        let sock = UdpSocket::bind(("0.0.0.0", *p)).expect("--sink bind");
        std::thread::spawn(move || {
            let mut b = [0u8; 2048];
            loop {
                let _ = sock.recv_from(&mut b);
            }
        });
    }
    let out = UdpSocket::bind("0.0.0.0:0").expect("udp out");
    let watch: Option<WatchList> = watch_bind.map(|b| {
        let list: WatchList = Default::default();
        let sock = UdpSocket::bind(&b).expect("--watch-bind");
        let l = list.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((n, _)) = sock.recv_from(&mut buf) else {
                    continue;
                };
                let now = Instant::now();
                let mut g = l.write().unwrap();
                for c in buf[..n].chunks_exact(64) {
                    let mut k = [0u8; 64];
                    k.copy_from_slice(c);
                    g.insert(k, now);
                }
                g.retain(|_, t| t.elapsed() < WATCH_KEEP);
            }
        });
        list
    });
    let mut ob = Observer {
        slots: HashMap::new(),
        fec: HashMap::new(),
        rs: ReedSolomonCache::default(),
        max_slot: 0,
        seen: HashSet::new(),
        seen_order: VecDeque::new(),
        out,
        emit,
        watch,
        st: Stats::default(),
    };
    eprintln!(
        "shred-observer: capturing incoming UDP to {} ports, emitting to {:?}",
        port_ok.iter().filter(|x| **x).count(),
        ob.emit
    );
    let mut buf = vec![0u8; 65536];
    let mut last = Instant::now();
    loop {
        let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        let mut sll_len = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
        let n = unsafe {
            libc::recvfrom(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
                &mut sll as *mut _ as *mut libc::sockaddr,
                &mut sll_len,
            )
        };
        if n <= 0 {
            continue;
        }
        let pkt = &buf[..n as usize];
        ob.st.packets += 1;
        // Only packets addressed to this host (not our own outgoing retransmits).
        if sll.sll_pkttype as i32 == libc::PACKET_OUTGOING as i32 {
            continue;
        }
        if pkt.len() < 28 || pkt[0] >> 4 != 4 || pkt[9] != 17 {
            continue; // not IPv4 UDP
        }
        let ihl = ((pkt[0] & 0x0f) as usize) * 4;
        let total = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
        if pkt.len() < ihl + 8 || total > pkt.len() || total < ihl + 8 {
            continue;
        }
        let dport = u16::from_be_bytes([pkt[ihl + 2], pkt[ihl + 3]]);
        if !port_ok[dport as usize] {
            continue;
        }
        let payload = &pkt[ihl + 8..total];
        if !SHRED_PACKET_SIZES.contains(&payload.len()) {
            continue;
        }
        if let Some(v) = shred_version {
            let got = u16::from_le_bytes([payload[VERSION_OFFSET], payload[VERSION_OFFSET + 1]]);
            if got != v {
                ob.st.bad_version += 1;
                continue;
            }
        }
        ob.st.udp_in_range += 1;
        match layout::get_shred_type(payload) {
            Ok(ShredType::Data) => ob.on_shred(payload, true),
            Ok(ShredType::Code) => {
                ob.st.code_shreds += 1;
                ob.on_shred(payload, false);
            }
            Err(_) => {}
        }
        if last.elapsed() >= Duration::from_secs(10) {
            last = Instant::now();
            let s = &ob.st;
            eprintln!(
                "stats packets={} shred_udp={} bad_version={} bad_slot={} data={} code={} dup={} recovered_sets={} recovered_data={} recover_err={} batches={} watch_hits={} deshred_err={} decode_err={} markers={} txs={} sigs_emitted={} send_err={} slots={} max_slot={}",
                s.packets, s.udp_in_range, s.bad_version, s.bad_slot, s.data_shreds, s.code_shreds, s.dup_shreds,
                s.recovered_sets, s.recovered_data, s.recover_err, s.batches, s.watch_hits,
                s.deshred_err, s.decode_err, s.markers, s.txs, s.sigs_emitted, s.send_err,
                ob.slots.len(), ob.max_slot
            );
        }
    }
}
