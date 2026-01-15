// Copyright 2024 V Kontakte LLC
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cmp;
use std::io::{Error, ErrorKind, Write};
use std::net::{Ipv4Addr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_ADDR: &str = "127.0.0.1:13337";
const TCP_HANDSHAKE: &[u8] = b"statshousev1";
const LENGTH_PREFIX_LEN: usize = 4;
const BATCH_TAG_POS: usize = LENGTH_PREFIX_LEN;
const BATCH_FIELD_MASK_POS: usize = BATCH_TAG_POS + 4;
const BATCH_COUNT_POS: usize = BATCH_FIELD_MASK_POS + 4;
const BATCH_HEADER_LEN: usize = BATCH_COUNT_POS + 4; // data length + TL tag + field mask + # of batches

#[cfg(target_os = "macos")]
const MAX_UDP_DATAGRAM_SIZE: usize = 9216; // sysctl net.inet.udp.maxdgram
#[cfg(not(target_os = "macos"))]
const MAX_UDP_DATAGRAM_SIZE: usize = 65507; // https://stackoverflow.com/questions/42609561/udp-maximum-packet-size/42610200
const MAX_TCP_PACKET_SIZE: usize = u16::MAX as usize;
const MAX_PACKET_SIZE: usize = MAX_TCP_PACKET_SIZE;
const MAX_FULL_KEY_SIZE: usize = 4096; // roughly metric plus all tags
const TL_MAX_TINY_STRING_LEN: usize = 253;
const TL_BIG_STRING_LEN: usize = 0x00ff_ffff;
const TL_BIG_STRING_MARKER: usize = 0xfe;
const TL_STATSHOUSE_METRICS_BATCH_TAG: u32 = 0x5658_0239;
const TL_STATSHOUSE_METRIC_COUNTER_FIELDS_MASK: u32 = 1 << 0;
const TL_STATSHOUSE_METRIC_VALUE_FIELDS_MASK: u32 = 1 << 1;
const TL_STATSHOUSE_METRIC_UNIQUE_FIELDS_MASK: u32 = 1 << 2;
const TL_STATSHOUSE_METRIC_TS_FIELDS_MASK: u32 = 1 << 4;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Network {
    Tcp,
    Udp,
}

enum TransportSocket {
    Udp(UdpSocket),
    Tcp(TcpStream),
}

pub struct Transport {
    socket: Result<TransportSocket, Error>,
    tl_buffer: TLBuffer<MAX_PACKET_SIZE>,
    batch_count: u32,
    tcp_pending: Vec<u8>,
    tcp_pending_pos: usize,
    tcp_pending_metrics: u32,
    last_flush: u32,
    stats: Stats,
    external_time: bool,
}

#[derive(Default, Copy, Clone)]
pub struct Stats {
    pub metrics_overflow: usize,
    pub metrics_failed: usize,
    pub metrics_too_big: usize,
    pub packets_overflow: usize,
    pub packets_failed: usize,
}

/// # Examples
///
/// ```
/// let mut t = statshouse::Transport::default();
/// statshouse::MetricBuilder::new(b"test").tag(b"0", b"staging").tag(b"1", b"test").write_count(&mut t, 1.0, 0);
/// let mut m = statshouse::MetricBuilder::new(b"test").tag(b"0", b"staging").tag(b"1", b"test").clone();
/// t.write_count(&m, 1.0, 0);
/// ```
impl Transport {
    pub fn new<A: ToSocketAddrs>(addr: A) -> Transport {
        Transport::new_with_network(Network::Udp, addr)
    }

    pub fn tcp<A: ToSocketAddrs>(addr: A) -> Transport {
        Transport::new_with_network(Network::Tcp, addr)
    }

    pub fn udp<A: ToSocketAddrs>(addr: A) -> Transport {
        Transport::new_with_network(Network::Udp, addr)
    }

    fn new_with_network<A: ToSocketAddrs>(network: Network, addr: A) -> Transport {
        let max_size = match network {
            Network::Tcp => MAX_TCP_PACKET_SIZE,
            Network::Udp => MAX_UDP_DATAGRAM_SIZE + LENGTH_PREFIX_LEN,
        };
        let mut tl_buffer = TLBuffer::new(BATCH_HEADER_LEN, max_size);
        write_u32(
            &mut tl_buffer.arr,
            BATCH_TAG_POS,
            TL_STATSHOUSE_METRICS_BATCH_TAG,
        ); // TL tag
        let socket = match network {
            Network::Tcp => create_tcp_stream(addr).map(TransportSocket::Tcp),
            Network::Udp => create_udp_socket(addr).map(TransportSocket::Udp),
        };
        Self {
            socket,
            tl_buffer,
            batch_count: 0,
            tcp_pending: Vec::new(),
            tcp_pending_pos: 0,
            tcp_pending_metrics: 0,
            last_flush: unix_time_now(),
            stats: Stats::default(),
            external_time: false,
        }
    }

    #[must_use]
    pub fn get_stats(&self) -> Stats {
        self.stats
    }

    pub fn clear_stats(&mut self) {
        self.stats = Stats::default();
    }

    pub fn set_external_time(&mut self, now: u32) {
        if self.external_time && self.last_flush == now {
            return;
        }
        self.external_time = true;
        self.flush(now);
    }

    pub fn write_count(&mut self, builder: &MetricBuilder, count: f64, mut timestamp: u32) -> bool {
        if count <= 0. {
            return false;
        }
        if builder.tl_buffer_overflow {
            self.stats.metrics_too_big += 1;
            return false;
        }
        let mut len: usize = 4 + builder.tl_buffer.pos + 8; // field mask + header + counter
        let mut field_mask: u32 = TL_STATSHOUSE_METRIC_COUNTER_FIELDS_MASK;
        let now = self.time_now();
        if timestamp == 0 {
            timestamp = now;
        }
        if timestamp != 0 {
            field_mask |= TL_STATSHOUSE_METRIC_TS_FIELDS_MASK;
            len += 4;
        }
        if !self.ensure_enough_space(len, now) {
            return false;
        }
        self.tl_buffer
            .write_header_unchecked(field_mask, builder, count, timestamp);
        self.batch_count += 1;
        self.maybe_flush(now);
        true
    }

    pub fn write_value(&mut self, builder: &MetricBuilder, val: f64, timestamp: u32) -> bool {
        let vals: [f64; 1] = [val];
        self.write_values(builder, &vals, 0., timestamp)
    }

    pub fn write_values(
        &mut self,
        builder: &MetricBuilder,
        vals: &[f64],
        count: f64,
        mut timestamp: u32,
    ) -> bool {
        if builder.tl_buffer_overflow {
            self.stats.metrics_too_big += 1;
            return false;
        }
        let mut len: usize = 4 + builder.tl_buffer.pos + 4 + 8; // field mask + header + array length + single array value
        let mut field_mask: u32 = TL_STATSHOUSE_METRIC_VALUE_FIELDS_MASK;
        if should_write_counter(count, vals.len()) {
            field_mask |= TL_STATSHOUSE_METRIC_COUNTER_FIELDS_MASK;
            len += 8;
        }
        let now = self.time_now();
        if timestamp == 0 {
            timestamp = now;
        }
        if timestamp != 0 {
            field_mask |= TL_STATSHOUSE_METRIC_TS_FIELDS_MASK;
            len += 4;
        }
        let mut tail = vals;
        while !tail.is_empty() {
            if !self.ensure_enough_space(len, now) {
                return false;
            }
            self.tl_buffer
                .write_header_unchecked(field_mask, builder, count, timestamp);
            let space = self.tl_buffer.space_left().saturating_sub(4);
            tail = self
                .tl_buffer
                .write_values_unchecked(tail, cmp::min(space / 8, tail.len()));
            self.batch_count += 1;
        }
        self.maybe_flush(now);
        true
    }

    pub fn write_unique(&mut self, builder: &MetricBuilder, val: u64, timestamp: u32) -> bool {
        let vals: [u64; 1] = [val];
        self.write_uniques(builder, &vals, 0., timestamp)
    }

    pub fn write_uniques(
        &mut self,
        builder: &MetricBuilder,
        vals: &[u64],
        count: f64,
        mut timestamp: u32,
    ) -> bool {
        if builder.tl_buffer_overflow {
            self.stats.metrics_too_big += 1;
            return false;
        }
        let mut len: usize = 4 + builder.tl_buffer.pos + 4 + 8; // field mask + header + array length + single array value
        let mut field_mask: u32 = TL_STATSHOUSE_METRIC_UNIQUE_FIELDS_MASK;
        if should_write_counter(count, vals.len()) {
            field_mask |= TL_STATSHOUSE_METRIC_COUNTER_FIELDS_MASK;
            len += 8;
        }
        let now = self.time_now();
        if timestamp == 0 {
            timestamp = now;
        }
        if timestamp != 0 {
            field_mask |= TL_STATSHOUSE_METRIC_TS_FIELDS_MASK;
            len += 4;
        }
        let mut tail = vals;
        while !tail.is_empty() {
            if !self.ensure_enough_space(len, now) {
                return false;
            }
            self.tl_buffer
                .write_header_unchecked(field_mask, builder, count, timestamp);
            let space = self.tl_buffer.space_left().saturating_sub(4);
            tail = self
                .tl_buffer
                .write_uniques_unchecked(tail, cmp::min(space / 8, tail.len()));
            self.batch_count += 1;
        }
        self.maybe_flush(now);
        true
    }

    fn ensure_enough_space(&mut self, len: usize, now: u32) -> bool {
        if self.tl_buffer.enough_space(len) {
            return true;
        }
        if self.tl_buffer.pos == BATCH_HEADER_LEN {
            return false;
        }
        self.flush(now);
        self.tl_buffer.enough_space(len)
    }

    fn maybe_flush(&mut self, now: u32) {
        if self.last_flush != now {
            self.flush(now);
        }
    }

    fn flush(&mut self, now: u32) {
        self.last_flush = now;
        if self.batch_count == 0 && self.tcp_pending.is_empty() {
            return;
        }
        let Transport {
            socket,
            tl_buffer,
            batch_count,
            tcp_pending,
            tcp_pending_pos,
            tcp_pending_metrics,
            stats,
            ..
        } = self;
        let Some(socket) = socket.as_mut().ok() else {
            return;
        };
        match socket {
            TransportSocket::Udp(sock) => flush_udp(sock, batch_count, tl_buffer, stats),
            TransportSocket::Tcp(stream) => flush_tcp(
                stream,
                batch_count,
                tl_buffer,
                stats,
                tcp_pending,
                tcp_pending_pos,
                tcp_pending_metrics,
            ),
        }
    }

    fn time_now(&self) -> u32 {
        if self.external_time {
            return self.last_flush;
        }
        unix_time_now()
    }
}

enum PendingState {
    Drained,
    Pending,
    Error,
}

fn reset_current_batch(batch_count: &mut u32, tl_buffer: &mut TLBuffer<MAX_PACKET_SIZE>) {
    *batch_count = 0;
    tl_buffer.pos = BATCH_HEADER_LEN;
}

fn drop_current_batch_overflow(
    batch_count: &mut u32,
    tl_buffer: &mut TLBuffer<MAX_PACKET_SIZE>,
    stats: &mut Stats,
) {
    stats.packets_overflow += 1;
    stats.metrics_overflow += *batch_count as usize;
    reset_current_batch(batch_count, tl_buffer);
}

fn drop_current_batch_failed(
    batch_count: &mut u32,
    tl_buffer: &mut TLBuffer<MAX_PACKET_SIZE>,
    stats: &mut Stats,
) {
    stats.packets_failed += 1;
    stats.metrics_failed += *batch_count as usize;
    reset_current_batch(batch_count, tl_buffer);
}

fn drain_tcp_pending(
    stream: &mut TcpStream,
    pending: &mut Vec<u8>,
    pending_pos: &mut usize,
) -> PendingState {
    let buf = &pending[*pending_pos..];
    if let Ok(written) = write_tcp(stream, buf) {
        *pending_pos += written;
        if *pending_pos < pending.len() {
            return PendingState::Pending;
        }
        pending.clear();
        *pending_pos = 0;
        PendingState::Drained
    } else {
        pending.clear();
        *pending_pos = 0;
        PendingState::Error
    }
}

fn flush_udp(
    sock: &UdpSocket,
    batch_count: &mut u32,
    tl_buffer: &mut TLBuffer<MAX_PACKET_SIZE>,
    stats: &mut Stats,
) {
    if *batch_count == 0 {
        return;
    }
    write_u32(&mut tl_buffer.arr, BATCH_COUNT_POS, *batch_count);
    let Ok(payload_len) = u32::try_from(tl_buffer.pos.saturating_sub(LENGTH_PREFIX_LEN)) else {
        drop_current_batch_failed(batch_count, tl_buffer, stats);
        return;
    };
    write_u32(&mut tl_buffer.arr, 0, payload_len);
    if let Err(ref e) = sock.send(&tl_buffer.arr[LENGTH_PREFIX_LEN..tl_buffer.pos]) {
        if e.kind() == ErrorKind::WouldBlock {
            drop_current_batch_overflow(batch_count, tl_buffer, stats);
            return;
        }
        drop_current_batch_failed(batch_count, tl_buffer, stats);
        return;
    }
    reset_current_batch(batch_count, tl_buffer);
}

fn flush_tcp(
    stream: &mut TcpStream,
    batch_count: &mut u32,
    tl_buffer: &mut TLBuffer<MAX_PACKET_SIZE>,
    stats: &mut Stats,
    tcp_pending: &mut Vec<u8>,
    tcp_pending_pos: &mut usize,
    tcp_pending_metrics: &mut u32,
) {
    if !tcp_pending.is_empty() {
        match drain_tcp_pending(stream, tcp_pending, tcp_pending_pos) {
            PendingState::Pending => {
                if *batch_count != 0 {
                    drop_current_batch_overflow(batch_count, tl_buffer, stats);
                }
                return;
            }
            PendingState::Error => {
                stats.packets_failed += 1;
                stats.metrics_failed += *tcp_pending_metrics as usize;
                *tcp_pending_metrics = 0;
                if *batch_count != 0 {
                    drop_current_batch_failed(batch_count, tl_buffer, stats);
                }
                return;
            }
            PendingState::Drained => {
                *tcp_pending_metrics = 0;
            }
        }
    }
    if *batch_count == 0 {
        return;
    }
    write_u32(&mut tl_buffer.arr, BATCH_COUNT_POS, *batch_count);
    let Ok(payload_len) = u32::try_from(tl_buffer.pos.saturating_sub(LENGTH_PREFIX_LEN)) else {
        drop_current_batch_failed(batch_count, tl_buffer, stats);
        return;
    };
    write_u32(&mut tl_buffer.arr, 0, payload_len);
    let buf = &tl_buffer.arr[..tl_buffer.pos];
    if let Ok(written) = write_tcp(stream, buf) {
        if written < buf.len() {
            tcp_pending.clear();
            tcp_pending.extend_from_slice(buf);
            *tcp_pending_pos = written;
            *tcp_pending_metrics = *batch_count;
        }
        reset_current_batch(batch_count, tl_buffer);
    } else {
        drop_current_batch_failed(batch_count, tl_buffer, stats);
    }
}

impl Default for Transport {
    fn default() -> Self {
        Transport::new(DEFAULT_ADDR)
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.flush(0);
    }
}

#[derive(Clone)]
pub struct MetricBuilder {
    tl_buffer: TLBuffer<MAX_FULL_KEY_SIZE>,
    tl_buffer_overflow: bool,
    tag_count: u32,
    tag_count_pos: usize,
}

impl MetricBuilder {
    #[must_use]
    pub fn new(metric_name: &[u8]) -> MetricBuilder {
        let mut m = MetricBuilder {
            tl_buffer: TLBuffer::new(0, MAX_FULL_KEY_SIZE),
            tl_buffer_overflow: false,
            tag_count: 0,
            tag_count_pos: 0,
        };
        if m.tl_buffer.write_string(metric_name) {
            m.tag_count_pos = m.tl_buffer.pos;
            m.tl_buffer_overflow = !m.tl_buffer.write_u32(0);
        } else {
            m.tl_buffer_overflow = true;
        }
        m
    }

    pub fn tag<'a>(&'a mut self, name: &[u8], value: &[u8]) -> &'a mut Self {
        if self.tl_buffer.write_string(name) && self.tl_buffer.write_string(value) {
            self.tag_count += 1;
        } else {
            self.tl_buffer_overflow = true;
        }
        self
    }

    pub fn write_count(&self, t: &mut Transport, count: f64, timestamp: u32) -> bool {
        t.write_count(self, count, timestamp)
    }

    pub fn write_value(&self, t: &mut Transport, val: f64, timestamp: u32) -> bool {
        t.write_value(self, val, timestamp)
    }

    pub fn write_values(
        &self,
        t: &mut Transport,
        vals: &[f64],
        count: f64,
        timestamp: u32,
    ) -> bool {
        t.write_values(self, vals, count, timestamp)
    }

    pub fn write_unique(&self, t: &mut Transport, val: u64, timestamp: u32) -> bool {
        t.write_unique(self, val, timestamp)
    }

    pub fn write_uniques(
        &self,
        t: &mut Transport,
        vals: &[u64],
        count: f64,
        timestamp: u32,
    ) -> bool {
        t.write_uniques(self, vals, count, timestamp)
    }
}

#[derive(Copy, Clone)]
struct TLBuffer<const N: usize> {
    arr: [u8; N],
    pos: usize,
    limit: usize,
}

impl<const N: usize> TLBuffer<N> {
    fn new(pos: usize, limit: usize) -> TLBuffer<N> {
        TLBuffer {
            arr: [0; N],
            pos,
            limit: limit.min(N),
        }
    }

    fn write_header_unchecked(
        &mut self,
        field_mask: u32,
        builder: &MetricBuilder,
        count: f64,
        timestamp: u32,
    ) {
        let dst = &mut self.arr;
        let mut pos = self.pos;
        // field mask
        write_u32(dst, pos, field_mask);
        pos += 4;
        // metric name, tag count and tag values
        write_slice(dst, pos, &builder.tl_buffer.arr[0..builder.tl_buffer.pos]);
        write_u32(dst, pos + builder.tag_count_pos, builder.tag_count);
        pos += builder.tl_buffer.pos;
        // counter
        if field_mask & TL_STATSHOUSE_METRIC_COUNTER_FIELDS_MASK != 0 {
            write_u64(dst, pos, count.to_bits());
            pos += 8;
        }
        // timestamp
        if field_mask & TL_STATSHOUSE_METRIC_TS_FIELDS_MASK != 0 {
            write_u32(dst, pos, timestamp);
            pos += 4;
        }
        self.pos = pos;
    }

    fn write_values_unchecked<'a>(&mut self, values: &'a [f64], count: usize) -> &'a [f64] {
        let dst = &mut self.arr;
        let mut pos = self.pos;
        #[allow(clippy::cast_possible_truncation)]
        write_u32(dst, pos, count as u32);
        pos += 4;
        for val in values.iter().take(count) {
            write_u64(dst, pos, val.to_bits());
            pos += 8;
        }
        self.pos = pos;
        &values[count..]
    }

    fn write_uniques_unchecked<'a>(&mut self, values: &'a [u64], count: usize) -> &'a [u64] {
        let dst = &mut self.arr;
        let mut pos = self.pos;
        #[allow(clippy::cast_possible_truncation)]
        write_u32(dst, pos, count as u32);
        pos += 4;
        for val in values.iter().take(count) {
            write_u64(dst, pos, *val);
            pos += 8;
        }
        self.pos = pos;
        &values[count..]
    }

    fn write_string(&mut self, str: &[u8]) -> bool {
        let mut len = str.len();
        if len > TL_MAX_TINY_STRING_LEN {
            if len > TL_BIG_STRING_LEN {
                len = TL_BIG_STRING_LEN;
            }
            let full_len = (4 + len + 3) & !3;
            if !self.enough_space(full_len) {
                return false;
            }
            write_u32(&mut self.arr, self.pos + full_len - 4, 0); // padding
            #[allow(clippy::cast_possible_truncation)]
            write_u32(
                &mut self.arr,
                self.pos,
                ((len << 8) | TL_BIG_STRING_MARKER) as u32,
            );
            write_slice(&mut self.arr, self.pos + 4, str);
            self.pos += full_len;
        } else {
            let full_len = (1 + len + 3) & !3;
            if !self.enough_space(full_len) {
                return false;
            }
            write_u32(&mut self.arr, self.pos + full_len - 4, 0); // padding
            #[allow(clippy::cast_possible_truncation)]
            let len8 = len as u8;
            self.arr[self.pos] = len8;
            write_slice(&mut self.arr, self.pos + 1, str); // string
            self.pos += full_len;
        }
        true
    }

    fn write_u32(&mut self, v: u32) -> bool {
        if !self.enough_space(4) {
            return false;
        }
        write_u32(&mut self.arr, self.pos, v);
        self.pos += 4;
        true
    }

    fn enough_space(&self, required: usize) -> bool {
        required <= self.space_left()
    }

    fn space_left(&self) -> usize {
        self.limit.saturating_sub(self.pos)
    }
}

fn create_udp_socket<A: ToSocketAddrs>(addr: A) -> Result<UdpSocket, Error> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_nonblocking(true)?;
    socket.connect(addr)?;
    Ok(socket)
}

fn create_tcp_stream<A: ToSocketAddrs>(addr: A) -> Result<TcpStream, Error> {
    let mut stream = TcpStream::connect(addr)?;
    stream.write_all(TCP_HANDSHAKE)?;
    stream.set_nonblocking(true)?;
    Ok(stream)
}

fn write_tcp(stream: &mut TcpStream, buf: &[u8]) -> Result<usize, Error> {
    let mut offset = 0;
    while offset < buf.len() {
        match stream.write(&buf[offset..]) {
            Ok(0) => {
                return Err(Error::new(
                    ErrorKind::WriteZero,
                    "failed to write to TCP stream",
                ))
            }
            Ok(n) => offset += n,
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => return Ok(offset),
            Err(err) => return Err(err),
        }
    }
    Ok(offset)
}

fn unix_time_now() -> u32 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        #[allow(clippy::cast_possible_truncation)]
        Ok(d) => d.as_secs() as u32,
        Err(_) => 0,
    }
}

fn should_write_counter(count: f64, len: usize) -> bool {
    if count.is_nan() || count < 0.0 {
        return false;
    }
    let Ok(len_u32) = u32::try_from(len) else {
        return true;
    };
    let len_f = f64::from(len_u32);
    (count - len_f).abs() > f64::EPSILON
}

fn write_u32<const N: usize>(dst: &mut [u8; N], pos: usize, val: u32) {
    dst[pos..pos + 4].copy_from_slice(&val.to_le_bytes());
}

fn write_u64<const N: usize>(dst: &mut [u8; N], pos: usize, val: u64) {
    dst[pos..pos + 8].copy_from_slice(&val.to_le_bytes());
}

fn write_slice<const N: usize>(dst: &mut [u8; N], pos: usize, val: &[u8]) {
    dst[pos..pos + val.len()].copy_from_slice(val);
}
