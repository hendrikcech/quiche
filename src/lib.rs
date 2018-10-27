// Copyright (c) 2018, Alessandro Ghedini
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright
//       notice, this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

#[macro_use]
extern crate log;
extern crate core;
extern crate libc;
extern crate ring;

#[macro_use]
extern crate lazy_static;

use std::cmp;
use std::mem;
use std::collections::HashMap;

pub const VERSION_DRAFT15: u32 = 0xff00000f;

pub const CLIENT_INITIAL_MIN_LEN: usize = 1200;

const MAX_PKT_LEN: usize = 1252;

pub type Result<T> = ::std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    WrongForm,
    UnknownVersion,
    UnknownPacket,
    UnknownFrame,
    UnknownStream,
    BufferTooShort,
    InvalidPacket,
    InvalidState,
    CryptoFail,
    TlsFail,
    Again,
    NothingToDo,
    FlowControl,
}

#[derive(Clone, Copy, Debug)]
pub struct Config<'a> {
    pub version: u32,

    pub local_conn_id: &'a [u8],

    pub local_transport_params: &'a TransportParams,

    pub tls_server_name: &'a str,
    pub tls_certificate: &'a str,
    pub tls_certificate_key: &'a str,
}

pub struct Conn {
    version: u32,

    dcid: Vec<u8>,
    scid: Vec<u8>,

    initial: packet::PktNumSpace,
    handshake: packet::PktNumSpace,
    application: packet::PktNumSpace,

    peer_transport_params: TransportParams,

    local_transport_params: TransportParams,

    tls_state: tls::State,

    rx_data: usize,
    max_rx_data: usize,

    tx_data: usize,
    max_tx_data: usize,

    streams: HashMap<u64, stream::Stream>,

    is_server: bool,

    derived_initial_secrets: bool,

    sent_initial: bool,

    got_peer_conn_id: bool,

    handshake_completed: bool,

    draining: bool,
}

impl Conn {
    pub fn new(config: Config, is_server: bool) -> Result<Box<Conn>> {
        Conn::new_with_tls(config, tls::State::new(), is_server)
    }

    fn new_with_tls(config: Config, tls: tls::State, is_server: bool)
                                                    -> Result<Box<Conn>> {
        let mut conn = Box::new(Conn {
            version: config.version,

            dcid: Vec::new(),
            scid: config.local_conn_id.to_vec(),

            initial: packet::PktNumSpace::new(packet::Type::Initial,
                                              crypto::Level::Initial),
            handshake: packet::PktNumSpace::new(packet::Type::Handshake,
                                                crypto::Level::Handshake),
            application: packet::PktNumSpace::new(packet::Type::Application,
                                                  crypto::Level::Application),

            peer_transport_params: TransportParams::default(),

            local_transport_params: config.local_transport_params.clone(),

            tls_state: tls,

            rx_data: 0,
            max_rx_data: config.local_transport_params.initial_max_data as usize,

            tx_data: 0,
            max_tx_data: 0,

            streams: HashMap::new(),

            is_server,

            derived_initial_secrets: false,

            sent_initial: false,

            got_peer_conn_id: false,

            handshake_completed: false,

            draining: false,
        });

        conn.tls_state.init_with_conn_extra(&conn, &config)
                      .map_err(|_e| Error::TlsFail)?;

        // Derive initial secrets for the client. We can do this here because
        // we randomly generate the destination connection ID used in the
        // secrets derivation.
        if !is_server {
            let mut dcid: [u8; 16] = [0; 16];
            rand::rand_bytes(&mut dcid[..]);

            let (aead_open, aead_seal) =
                crypto::derive_initial_key_material(&dcid, conn.is_server)?;

            conn.dcid.extend_from_slice(&dcid);

            conn.initial.crypto_open = Some(aead_open);
            conn.initial.crypto_seal = Some(aead_seal);

            conn.derived_initial_secrets = true;
        }

        Ok(conn)
    }

    pub fn recv(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Err(Error::BufferTooShort);
        }

        self.do_handshake()?;

        let mut b = octets::Bytes::new(buf);

        let hdr = if !packet::has_long_header(b.peek_u8()?) {
            packet::Header::short_from_bytes(&mut b, self.scid.len())?
        } else {
            let hdr = packet::Header::long_from_bytes(&mut b)?;

            if hdr.version != self.version {
                return Err(Error::UnknownVersion);
            }

            hdr
        };

        // Long header packets have an explicit payload length, but short
        // packets don't so just use the remaining capacity in the buffer.
        let payload_len = if hdr.ty == packet::Type::Application {
            b.cap()
        } else  {
            b.get_varint()? as usize
        };

        if b.cap() < payload_len {
            return Err(Error::BufferTooShort);
        }

        if !self.is_server && !self.got_peer_conn_id {
            // Replace the randomly generated destination connection ID with
            // the one supplied by the server.
            self.dcid.resize(hdr.scid.len(), 0);
            self.dcid.copy_from_slice(&hdr.scid);

            self.got_peer_conn_id = true;
        }

        // Derive initial secrets.
        if !self.derived_initial_secrets {
            let (aead_open, aead_seal) =
                crypto::derive_initial_key_material(&hdr.dcid,
                                                    self.is_server)?;

            self.initial.crypto_open = Some(aead_open);
            self.initial.crypto_seal = Some(aead_seal);

            self.derived_initial_secrets = true;

            self.dcid.extend_from_slice(&hdr.scid);
            self.got_peer_conn_id = true;
        }

        let trace_id = self.trace_id();

        // Select packet number space context.
        let space = match hdr.ty {
            packet::Type::Initial => &mut self.initial,

            packet::Type::Handshake => &mut self.handshake,

            packet::Type::Application => &mut self.application,

            _ => return Err(Error::InvalidPacket),
        };

        let aead = match space.crypto_open {
            Some(ref v) => v,
            None        => return Err(Error::InvalidState),
        };

        let (pn, pn_len) = packet::decrypt_pkt_num(&mut b, &aead)?;
        b.skip(pn_len)?;

        let pn = packet::decode_pkt_num(space.largest_rx_pkt_num, pn, pn_len)?;

        trace!("{} rx pkt {:?} len={} pn={}", trace_id, hdr, payload_len, pn);

        let payload_offset = b.off();

        let (header, mut payload) = b.split_at(payload_offset)?;

        let payload_len = {
            let mut ciphertext = payload.peek_bytes(payload_len - pn_len)?;
            packet::decrypt_pkt(ciphertext.as_mut(), pn, header.as_ref(), &aead)?
        };

        let mut payload = payload.get_bytes(payload_len)?;

        // To avoid sending an ACK in response to an ACK-only packet, we need
        // to keep track of whether this packet contains any frame other than
        // ACK.
        let mut do_ack = false;

        // Process packet payload.
        while payload.cap() > 0 {
            let frame = frame::Frame::from_bytes(&mut payload)?;

            trace!("{} rx frm {:?}", trace_id, frame);

            match frame {
                frame::Frame::Padding { .. } => (),

                frame::Frame::ConnectionClose { .. } => {
                    self.draining = true;
                },

                frame::Frame::ApplicationClose { .. } => {
                    self.draining = true;
                },

                frame::Frame::MaxData { max } => {
                    self.max_tx_data = cmp::max(self.max_tx_data,
                                                max as usize);

                    do_ack = true;
                },

                frame::Frame::MaxStreamData { stream_id, max } => {
                    let stream = match self.streams.get_mut(&stream_id) {
                        Some(v) => v,
                        None => return Err(Error::UnknownStream),
                    };

                    stream.max_tx_data = cmp::max(stream.max_tx_data,
                                                  max as usize);

                    do_ack = true;
                },

                frame::Frame::MaxStreamId { .. } => {
                    do_ack = true;
                },

                frame::Frame::Ping => {
                    do_ack = true;
                },

                frame::Frame::NewConnectionId { .. } => {
                    do_ack = true;
                },

                frame::Frame::RetireConnectionId { .. } => {
                    do_ack = true;
                },

                // TODO: implement ack and retransmission.
                frame::Frame::ACK { .. } => (),

                // TODO: implement stateless retry
                frame::Frame::NewToken { .. } => {
                    do_ack = true;
                },

                frame::Frame::Crypto { data } => {
                    // Push the data to the stream so it can be re-ordered.
                    space.crypto_stream.push_recv(data)?;

                    // Feed crypto data to the TLS state, if there's data
                    // available at the expected offset.
                    if space.crypto_stream.can_read() {
                        let buf = space.crypto_stream.pop_recv()?;
                        let level = space.crypto_level;


                        self.tls_state.provide_data(level, &buf)
                                      .map_err(|_e| Error::TlsFail)?;
                    }

                    do_ack = true;
                },

                frame::Frame::Stream { stream_id, data } => {
                    let max_rx_data = self.local_transport_params
                                          .initial_max_stream_data_bidi_local as usize;
                    let max_tx_data = self.peer_transport_params
                                          .initial_max_stream_data_bidi_remote as usize;

                    // Get existing stream or create a new one.
                    let stream = self.streams.entry(stream_id).or_insert_with(|| {
                        stream::Stream::new(max_rx_data, max_tx_data)
                    });

                    stream.rx_data = cmp::max(stream.rx_data, data.off());

                    if stream.tx_data > stream.max_rx_data {
                        return Err(Error::FlowControl);
                    }

                    stream.push_recv(data)?;

                    do_ack = true;
                },
            }
        }

        space.recv_pkt_num.push_item(pn);
        space.do_ack = cmp::max(space.do_ack, do_ack);

        space.largest_rx_pkt_num = cmp::max(space.largest_rx_pkt_num, pn);

        let read = payload_offset + payload_len + aead.tag_len();
        Ok(read)
    }

    pub fn send(&mut self, out: &mut [u8]) -> Result<usize> {
        if out.is_empty() {
            return Err(Error::BufferTooShort);
        }

        if self.draining {
            return Err(Error::NothingToDo);
        }

        self.do_handshake()?;

        let max_pkt_len = self.peer_transport_params.max_packet_size as usize;

        // Cap output buffer to respect peer's max_packet_size limit.
        let avail = cmp::min(max_pkt_len, out.len());

        let mut b = octets::Bytes::new(&mut out[..avail]);

        let trace_id = self.trace_id();

        // Select packet number space context depending on whether there is
        // handshake data to send, whether there are packets to ACK, or in
        // the case of the application space, whether there are streams that
        // can be written or that needs to increase flow control credit.
        let space =
            if self.initial.crypto_stream.can_write() ||
               self.initial.do_ack {
                &mut self.initial
            } else if self.handshake.crypto_stream.can_write() ||
                      self.handshake.do_ack {
                &mut self.handshake
            } else if self.handshake_completed &&
                      (self.application.crypto_stream.can_write() ||
                       self.application.do_ack ||
                       self.streams.values().any(|s| s.can_write()) ||
                       self.streams.values().any(|s| s.more_credit())) {
                &mut self.application
            } else {
                return Err(Error::NothingToDo);
            };

        let hdr = packet::Header {
            ty: space.pkt_type,
            version: self.version,
            flags: 0,
            dcid: self.dcid.clone(),
            scid: self.scid.clone(),
            token: None,
        };

        if space.pkt_type == packet::Type::Application {
            packet::Header::short_to_bytes(&hdr, &mut b)?;
        } else {
            packet::Header::long_to_bytes(&hdr, &mut b)?;
        }

        let pn = space.last_pkt_num;
        let pn_len = packet::pkt_num_len(pn)?;

        space.last_pkt_num += 1;

        // Calculate payload length.
        let mut length = pn_len + space.overhead();

        // Calculate remaining available space for the payload, excluding
        // payload length, pkt num and AEAD oerhead.
        let mut left = b.cap() - 4 - length;

        let mut frames: Vec<frame::Frame> = Vec::new();

        // Create ACK frame.
        if space.do_ack {
            let frame = frame::Frame::ACK {
                ack_delay: 0,
                ranges: space.recv_pkt_num.clone(),
            };

            space.recv_pkt_num.clear();
            space.do_ack = false;

            length += frame.wire_len();
            left -= frame.wire_len();

            frames.push(frame);
        }

        // Create CRYPTO frame.
        if space.crypto_stream.can_write() {
            let crypto_len = left - frame::MAX_CRYPTO_OVERHEAD;
            let crypto_buf = space.crypto_stream.pop_send(crypto_len)?;

            let frame = frame::Frame::Crypto {
                data: crypto_buf,
            };

            length += frame.wire_len();
            left -= frame.wire_len();

            frames.push(frame);
        }

        // Pad the client's initial packet.
        if !self.is_server && !self.sent_initial {
            let len: usize = cmp::min(CLIENT_INITIAL_MIN_LEN - length, left);

            let frame = frame::Frame::Padding {
                len,
            };

            length += frame.wire_len();
            left -= frame.wire_len();

            frames.push(frame);

            self.sent_initial = true;
        }

        // Create MAX_DATA frame.
        if space.pkt_type == packet::Type::Application {
            if self.rx_data + 2 * MAX_PKT_LEN > self.max_rx_data {
                let max = self.rx_data as u64 +
                          self.local_transport_params.initial_max_data as u64;

                let frame = frame::Frame::MaxData {
                    max: max as u64,
                };

                self.max_rx_data = max as usize;

                length += frame.wire_len();
                left -= frame.wire_len();

                frames.push(frame);
            }
        }

        // Create MAX_STREAM_DATA frame.
        if space.pkt_type == packet::Type::Application {
            for (id, stream) in &mut self.streams {
                if stream.more_credit() {
                    let max = stream.rx_data as u64 +
                              self.local_transport_params
                                  .initial_max_stream_data_bidi_local as u64;

                    let frame = frame::Frame::MaxStreamData {
                        stream_id: *id,
                        max: max as u64,
                    };

                    stream.max_rx_data = max as usize;

                    length += frame.wire_len();
                    left -= frame.wire_len();

                    frames.push(frame);
                }
            }
        }

        // Create STREAM frame.
        if space.pkt_type == packet::Type::Application &&
           self.tx_data != self.max_tx_data {
            for (id, stream) in &mut self.streams {
                if stream.can_write() {
                    if stream.tx_data == stream.max_tx_data {
                        trace!("{} stream {} is blocked", trace_id, id);
                        continue;
                    }

                    let max_tx_data = cmp::min(self.max_tx_data - self.tx_data,
                                               stream.max_tx_data - stream.tx_data);

                    let stream_len = cmp::min(max_tx_data,
                                              left - frame::MAX_STREAM_OVERHEAD);

                    if stream_len == 0 {
                        continue;
                    }

                    let stream_buf = stream.pop_send(stream_len)?;

                    let frame = frame::Frame::Stream {
                        stream_id: *id,
                        data: stream_buf,
                    };

                    length += frame.wire_len();

                    self.tx_data += stream_len;
                    stream.tx_data += stream_len;

                    frames.push(frame);
                    break;
                }
            }
        }

        if frames.len() == 0 {
            return Err(Error::NothingToDo);
        }

        // Only long header packets have an explicit length field.
        if space.pkt_type != packet::Type::Application {
            b.put_varint(length as u64)?;
        }

        packet::encode_pkt_num(pn, &mut b)?;

        let payload_len = length - pn_len;

        trace!("{} tx pkt {:?} len={} pn={}", trace_id, hdr, payload_len, pn);

        let payload_offset = b.off();

        for frame in &frames {
            trace!("{} tx frm {:?}", trace_id, frame);

            frame.to_bytes(&mut b)?;
        }

        let aead = match space.crypto_seal {
            Some(ref v) => v,
            None        => return Err(Error::InvalidState),
        };

        let (mut header, mut payload) = b.split_at(payload_offset)?;

        let ciphertext = payload.slice(payload_len)?;
        packet::encrypt_pkt(ciphertext, pn, header.as_ref(), aead)?;

        let sample = &ciphertext[4 - pn_len..16 + (4 - pn_len)];
        let pn_ciphertext = header.slice_last(pn_len)?;
        aead.xor_keystream(sample, pn_ciphertext)?;

        let written = payload_offset + payload_len;
        Ok(written)
    }

    pub fn stream_recv(&mut self, stream_id: u64) -> Result<stream::RangeBuf> {
        let stream = match self.streams.get_mut(&stream_id) {
            Some(v) => v,
            None => return Err(Error::UnknownStream),
        };

        if !stream.can_read() {
            return Ok(stream::RangeBuf::default());
        }

        stream.pop_recv()
    }

    pub fn stream_send(&mut self, stream_id: u64, buf: &[u8], fin: bool)
                                                            -> Result<usize> {
        let max_rx_data = self.local_transport_params
                              .initial_max_stream_data_bidi_local as usize;
        let max_tx_data = self.peer_transport_params
                              .initial_max_stream_data_bidi_remote as usize;

        let stream = self.streams.entry(stream_id).or_insert_with(|| {
            stream::Stream::new(max_rx_data, max_tx_data)
        });

        // TODO: implement backpressure based on peer's flow control

        stream.push_send(buf, fin)?;

        Ok(buf.len())
    }

    pub fn stream_iter(&mut self) -> stream::StreamIterator {
        stream::StreamIterator::new(self.streams.iter())
    }

    pub fn local_conn_id(&self) -> &[u8] {
        self.scid.as_slice()
    }

    pub fn trace_id(&self) -> String {
        let cid = self.local_conn_id();

        let vec: Vec<String> = cid.iter()
                                  .map(|b| format!("{:02x}", b))
                                  .collect();

        vec.join("")
    }

    pub fn is_established(&self) -> bool {
        self.handshake_completed
    }

    fn do_handshake(&mut self) -> Result<()> {
        if !self.handshake_completed {
            match self.tls_state.do_handshake() {
                Ok(_) => {
                    // Handshake is complete!
                    self.handshake_completed = true;

                    let mut raw_params =
                        self.tls_state.get_quic_transport_params()
                                      .map_err(|_e| Error::TlsFail)?;

                    let peer_params = TransportParams::decode(&mut raw_params,
                                                              self.version,
                                                              self.is_server)?;

                    self.peer_transport_params = peer_params;

                    self.max_tx_data =
                        self.peer_transport_params.initial_max_data as usize;

                    trace!("{} connection established: cipher={:?}",
                           self.trace_id(), self.application.cipher());
                },

                Err(tls::Error::TlsFail)          => return Err(Error::TlsFail),
                Err(tls::Error::WantRead)         => (), // continue
                Err(tls::Error::WantWrite)        => (), // continue
                Err(tls::Error::SyscallFail)      => return Err(Error::TlsFail),
                Err(tls::Error::PendingOperation) => return Err(Error::Again),
            }
        }

        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransportParams {
    pub idle_timeout: u16,
    pub initial_max_data: u32,
    pub initial_max_bidi_streams: u16,
    pub initial_max_uni_streams: u16,
    pub max_packet_size: u16,
    pub ack_delay_exponent: u8,
    pub disable_migration: bool,
    pub max_ack_delay: u8,
    pub initial_max_stream_data_bidi_local: u32,
    pub initial_max_stream_data_bidi_remote: u32,
    pub initial_max_stream_data_uni: u32,
    pub stateless_reset_token_present: bool,
    pub stateless_reset_token: [u8; 16],
    // pub preferred_address: ...
}

impl TransportParams {
    fn decode(buf: &mut [u8], _version: u32, is_server: bool)
                                                -> Result<TransportParams> {
        let mut b = octets::Bytes::new(buf);

        // TODO: check version
        let _tp_version = b.get_u32()?;

        if !is_server {
            // Ignore supported versions from server.
            b.get_bytes_with_u8_length()?;
        }

        let mut tp = TransportParams::default();

        let mut params = b.get_bytes_with_u16_length()?;

        while params.cap() > 0 {
            let id = params.get_u16()?;

            let mut val = params.get_bytes_with_u16_length()?;

            match id {
                0x0000 => {
                    tp.initial_max_stream_data_bidi_local = val.get_u32()?;
                },

                0x0001 => {
                    tp.initial_max_data = val.get_u32()?;
                },

                0x0002 => {
                    tp.initial_max_bidi_streams = val.get_u16()?;
                },

                0x0003 => {
                    tp.idle_timeout = val.get_u16()?;
                },

                0x0004 => {
                    // TODO: parse preferred_address
                },

                0x0005 => {
                    tp.max_packet_size = val.get_u16()?;
                },

                0x0006 => {
                    let token = val.get_bytes(16)?;
                    tp.stateless_reset_token.copy_from_slice(token.as_ref());
                    tp.stateless_reset_token_present = true;
                },

                0x0007 => {
                    tp.ack_delay_exponent = val.get_u8()?;
                },

                0x0008 => {
                    tp.initial_max_uni_streams = val.get_u16()?;
                },

                0x0009 => {
                    tp.disable_migration = true;
                },

                0x000a => {
                    tp.initial_max_stream_data_bidi_remote = val.get_u32()?;
                },

                0x000b => {
                    tp.initial_max_stream_data_uni = val.get_u32()?;
                },

                0x000c => {
                    tp.max_ack_delay = val.get_u8()?;
                },

                // Ignore unknown parameters.
                _ => (),
            }
        }

        Ok(tp)
    }

    fn encode<'a>(tp: &TransportParams, version: u32, is_server: bool,
                  out: &'a mut [u8]) -> Result<&'a mut [u8]> {
        // TODO: implement put_with_length API for octets::Bytes to avoid this copy
        let mut params: [u8; 128] = [0; 128];

        let params_len = {
            let mut b = octets::Bytes::new(&mut params);

            if tp.idle_timeout != 0 {
                b.put_u16(0x0003)?;
                b.put_u16(mem::size_of::<u16>() as u16)?;
                b.put_u16(tp.idle_timeout)?;
            }

            if tp.initial_max_data != 0 {
                b.put_u16(0x0001)?;
                b.put_u16(mem::size_of::<u32>() as u16)?;
                b.put_u32(tp.initial_max_data)?;
            }

            if tp.initial_max_bidi_streams != 0 {
                b.put_u16(0x0002)?;
                b.put_u16(mem::size_of::<u16>() as u16)?;
                b.put_u16(tp.initial_max_bidi_streams)?;
            }

            if tp.initial_max_uni_streams != 0 {
                b.put_u16(0x0008)?;
                b.put_u16(mem::size_of::<u16>() as u16)?;
                b.put_u16(tp.initial_max_uni_streams)?;
            }

            if tp.max_packet_size != 0 {
                b.put_u16(0x0005)?;
                b.put_u16(mem::size_of::<u16>() as u16)?;
                b.put_u16(tp.max_packet_size)?;
            }

            if tp.ack_delay_exponent != 0 {
                b.put_u16(0x0007)?;
                b.put_u16(mem::size_of::<u8>() as u16)?;
                b.put_u8(tp.ack_delay_exponent)?;
            }

            if tp.disable_migration {
                b.put_u16(0x0009)?;
                b.put_u16(0)?;
            }

            if tp.initial_max_stream_data_bidi_local != 0 {
                b.put_u16(0x0000)?;
                b.put_u16(mem::size_of::<u32>() as u16)?;
                b.put_u32(tp.initial_max_stream_data_bidi_local)?;
            }

            if tp.initial_max_stream_data_bidi_remote != 0 {
                b.put_u16(0x000a)?;
                b.put_u16(mem::size_of::<u32>() as u16)?;
                b.put_u32(tp.initial_max_stream_data_bidi_remote)?;
            }

            if tp.initial_max_stream_data_uni != 0 {
                b.put_u16(0x000b)?;
                b.put_u16(mem::size_of::<u32>() as u16)?;
                b.put_u32(tp.initial_max_stream_data_uni)?;
            }

            if is_server && tp.stateless_reset_token_present {
                b.put_u16(0x0006)?;
                b.put_u16(tp.stateless_reset_token.len() as u16)?;
                b.put_bytes(&tp.stateless_reset_token)?;
            }

            b.off()
        };

        let out_len = {
            let mut b = octets::Bytes::new(out);

            b.put_u32(version)?;

            if is_server {
                b.put_u8(mem::size_of::<u32>() as u8)?;
                b.put_u32(version)?;
            };

            b.put_u16(params_len as u16)?;
            b.put_bytes(&params[..params_len])?;

            b.off()
        };

        Ok(&mut out[..out_len])
    }
}

impl Default for TransportParams {
    fn default() -> TransportParams {
        TransportParams {
            idle_timeout: 0,
            initial_max_data: 0,
            initial_max_bidi_streams: 0,
            initial_max_uni_streams: 0,
            max_packet_size: 1205,
            ack_delay_exponent: 3,
            disable_migration: false,
            max_ack_delay: 25,
            initial_max_stream_data_bidi_local: 0,
            initial_max_stream_data_bidi_remote: 0,
            initial_max_stream_data_uni: 0,
            stateless_reset_token_present: false,
            stateless_reset_token: [0; 16],
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_params() {
        let tp = TransportParams {
            idle_timeout: 30,
            initial_max_data: 424645563,
            initial_max_bidi_streams: 12231,
            initial_max_uni_streams: 18473,
            max_packet_size: 23421,
            ack_delay_exponent: 123,
            disable_migration: true,
            max_ack_delay: 25,
            initial_max_stream_data_bidi_local: 154323123,
            initial_max_stream_data_bidi_remote: 6587456,
            initial_max_stream_data_uni: 2461234,
            stateless_reset_token_present: true,
            stateless_reset_token: [0xba; 16],
        };

        let mut raw_params: [u8; 256] = [42; 256];
        let mut raw_params = TransportParams::encode(&tp, VERSION_DRAFT15, true,
                                              &mut raw_params).unwrap();
        assert_eq!(raw_params.len(), 96);

        let new_tp = TransportParams::decode(&mut raw_params, VERSION_DRAFT15,
                                             false).unwrap();

        assert_eq!(new_tp, tp);
    }

    fn create_conn(is_server: bool) -> Box<Conn> {
        let tp = TransportParams::default();

        let mut scid: [u8; 16] = [0; 16];
        rand::rand_bytes(&mut scid[..]);

        let config = Config {
            version: VERSION_DRAFT15,

            local_conn_id: &scid,

            local_transport_params: &tp,

            tls_server_name: "quic.tech",
            tls_certificate: "examples/cert.crt",
            tls_certificate_key: "examples/cert.key",
        };

        Conn::new(config, is_server).unwrap()
    }

    fn recv_send(conn: &mut Conn, buf: &mut [u8], len: usize) -> usize {
        let mut left = len;

        while left > 0 {
            let read = conn.recv(&mut buf[len - left..len]).unwrap();

            left -= read;
        }

        let mut off = 0;

        while off < buf.len() {
            let write = match conn.send(&mut buf[off..]) {
                Ok(v)   => v,

                Err(Error::NothingToDo) => { break; },

                Err(e)  => panic!("SEND FAILED: {:?}", e),
            };

            off += write;
        }

        off
    }

    #[test]
    fn self_handshake() {
        let mut buf = [0; 65535];

        let mut cln = create_conn(false);
        let mut srv = create_conn(true);

        let mut len = cln.send(&mut buf).unwrap();

        while !cln.is_established() && !srv.is_established() {
            len = recv_send(&mut srv, &mut buf, len);
            len = recv_send(&mut cln, &mut buf, len);
        }

        assert!(true);
    }
}

pub mod packet;
pub mod rand;

mod crypto;
mod frame;
mod stream;
mod tls;
mod octets;
mod ranges;
