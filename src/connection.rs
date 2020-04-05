use futures::future;
use futures::future::BoxFuture;
use futures::pin_mut;
use futures::ready;
use futures::stream;
use futures::stream::BoxStream;
use futures::{Future, FutureExt, Sink, Stream, StreamExt};
use libnice::ice;
use mumble_protocol::control::msgs;
use mumble_protocol::control::ControlPacket;
use mumble_protocol::voice::VoicePacket;
use mumble_protocol::voice::VoicePacketPayload;
use mumble_protocol::Clientbound;
use mumble_protocol::Serverbound;
use openssl::asn1::Asn1Time;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::ssl::{SslAcceptor, SslAcceptorBuilder, SslMethod};
use openssl::x509::X509;
use rtp::rfc3550::{
    RtcpCompoundPacket, RtcpPacket, RtcpPacketReader, RtcpPacketWriter, RtpFixedHeader, RtpPacket,
    RtpPacketReader, RtpPacketWriter,
};
use rtp::rfc5761::{MuxPacketReader, MuxPacketWriter, MuxedPacket};
use rtp::rfc5764::DtlsSrtp;
use rtp::traits::{ReadPacket, WritePacket};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::net::IpAddr;
use std::ops::DerefMut;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use tokio::io;
use tokio::time::Delay;
use webrtc_sdp::attribute_type::SdpAttribute;

use crate::error::Error;
use crate::utils::EitherS;
use crate::Config;

type SessionId = u32;

struct User {
    session: u32,           // mumble session id
    ssrc: u32,              // ssrc id
    active: bool,           // whether the user is currently transmitting audio
    timeout: Option<Delay>, // assume end of transmission if silent until then
    start_voice_seq_num: u64,
    highest_voice_seq_num: u64,
    rtp_seq_num_offset: u32, // u32 because we also derive the timestamp from it
}

impl User {
    fn set_inactive(&mut self) -> impl Stream<Item = Result<Frame, Error>> {
        self.timeout = None;

        if self.active {
            self.active = false;

            self.rtp_seq_num_offset = self
                .rtp_seq_num_offset
                .wrapping_add((self.highest_voice_seq_num - self.start_voice_seq_num) as u32 + 1);
            self.start_voice_seq_num = 0;
            self.highest_voice_seq_num = 0;

            let mut msg = msgs::TalkingState::new();
            msg.set_session(self.session);
            EitherS::A(stream::once(future::ready(Ok(Frame::Client(msg.into())))))
        } else {
            EitherS::B(stream::empty())
        }
    }

    fn set_active(&mut self, target: u8) -> impl Stream<Item = Result<Frame, Error>> {
        self.timeout = Some(tokio::time::delay_for(Duration::from_millis(400)));

        if self.active {
            EitherS::A(stream::empty())
        } else {
            self.active = true;

            let mut msg = msgs::TalkingState::new();
            msg.set_session(self.session);
            msg.set_target(target.into());
            EitherS::B(stream::once(future::ready(Ok(Frame::Client(msg.into())))))
        }
    }
}

pub struct Connection {
    config: Config,
    inbound_client: Pin<Box<dyn Stream<Item = Result<ControlPacket<Serverbound>, Error>> + Send>>,
    outbound_client: Pin<Box<dyn Sink<ControlPacket<Clientbound>, Error = Error> + Send>>,
    inbound_server: Pin<Box<dyn Stream<Item = Result<ControlPacket<Clientbound>, Error>> + Send>>,
    outbound_server: Pin<Box<dyn Sink<ControlPacket<Serverbound>, Error = Error> + Send>>,
    next_clientbound_frame: Option<ControlPacket<Clientbound>>,
    next_serverbound_frame: Option<ControlPacket<Serverbound>>,
    next_rtp_frame: Option<Vec<u8>>,
    stream_to_be_sent: Option<BoxStream<'static, Result<Frame, Error>>>,

    ice: Option<(ice::Agent, ice::Stream)>,
    candidate_gathering_done: bool,

    dtls_srtp_future: Option<
        BoxFuture<'static, Result<DtlsSrtp<ice::StreamComponent, SslAcceptorBuilder>, io::Error>>,
    >,
    dtls_srtp: Option<DtlsSrtp<ice::StreamComponent, SslAcceptorBuilder>>,
    dtls_key: PKey<Private>,
    dtls_cert: X509,

    rtp_reader: MuxPacketReader<RtpPacketReader, RtcpPacketReader>,
    rtp_writer: MuxPacketWriter<RtpPacketWriter, RtcpPacketWriter>,

    target: Option<u8>, // only if client is talking
    next_ssrc: u32,
    free_ssrcs: Vec<u32>,
    sessions: BTreeMap<SessionId, User>,
}

impl Connection {
    pub fn new<CSi, CSt, SSi, SSt>(
        config: Config,
        client_sink: CSi,
        client_stream: CSt,
        server_sink: SSi,
        server_stream: SSt,
    ) -> Self
    where
        CSi: Sink<ControlPacket<Clientbound>, Error = Error> + 'static + Send,
        CSt: Stream<Item = Result<ControlPacket<Serverbound>, Error>> + 'static + Send,
        SSi: Sink<ControlPacket<Serverbound>, Error = Error> + 'static + Send,
        SSt: Stream<Item = Result<ControlPacket<Clientbound>, Error>> + 'static + Send,
    {
        let rsa = Rsa::generate(2048).unwrap();
        let key = PKey::from_rsa(rsa).unwrap();

        let mut cert_builder = X509::builder().unwrap();
        cert_builder
            .set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        cert_builder
            .set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        cert_builder.set_pubkey(&key).unwrap();
        cert_builder.sign(&key, MessageDigest::sha256()).unwrap();
        let cert = cert_builder.build();

        Self {
            config,
            inbound_client: Box::pin(client_stream),
            outbound_client: Box::pin(client_sink),
            inbound_server: Box::pin(server_stream),
            outbound_server: Box::pin(server_sink),
            next_clientbound_frame: None,
            next_serverbound_frame: None,
            next_rtp_frame: None,
            stream_to_be_sent: None,
            ice: None,
            candidate_gathering_done: false,
            dtls_srtp_future: None,
            dtls_srtp: None,
            dtls_key: key,
            dtls_cert: cert,
            rtp_reader: MuxPacketReader::new(RtpPacketReader, RtcpPacketReader),
            rtp_writer: MuxPacketWriter::new(RtpPacketWriter, RtcpPacketWriter),
            target: None,
            next_ssrc: 1,
            free_ssrcs: Vec::new(),
            sessions: BTreeMap::new(),
        }
    }

    fn supports_webrtc(&self) -> bool {
        self.ice.is_some()
    }

    fn allocate_ssrc(&mut self, session_id: SessionId) -> &mut User {
        let ssrc = self.free_ssrcs.pop().unwrap_or_else(|| {
            let ssrc = self.next_ssrc;
            self.next_ssrc += 1;
            if let Some(ref mut dtls_srtp) = self.dtls_srtp {
                dtls_srtp.add_incoming_unknown_ssrcs(1);
                dtls_srtp.add_outgoing_unknown_ssrcs(1);
            }
            ssrc
        });
        let user = User {
            session: session_id,
            ssrc,
            active: false,
            timeout: None,
            start_voice_seq_num: 0,
            highest_voice_seq_num: 0,
            rtp_seq_num_offset: 0,
        };
        self.sessions.insert(session_id, user);
        self.sessions.get_mut(&session_id).unwrap()
    }

    fn free_ssrc(&mut self, session_id: SessionId) {
        if let Some(user) = self.sessions.remove(&session_id) {
            self.free_ssrcs.push(user.ssrc)
        }
    }

    fn setup_ice(&mut self) -> impl Stream<Item = Result<Frame, Error>> {
        // Setup ICE agent
        let mut agent = ice::Agent::new_rfc5245();
        agent.set_software("mumble-web-proxy");
        agent.set_controlling_mode(true);

        // Setup ICE stream
        let mut stream = match {
            let mut builder = agent.stream_builder(1);
            if self.config.min_port != 1 || self.config.max_port != u16::max_value() {
                builder.set_port_range(self.config.min_port, self.config.max_port);
            }
            builder.build()
        } {
            Ok(stream) => stream,
            Err(err) => {
                return stream::once(future::ready(Err(io::Error::new(
                    io::ErrorKind::Other,
                    err,
                )
                .into())));
            }
        };
        let component = stream.take_components().pop().expect("one component");

        // Send WebRTC details to the client
        let mut msg = msgs::WebRTC::new();
        msg.set_dtls_fingerprint(
            self.dtls_cert
                .digest(MessageDigest::sha256())
                .unwrap()
                .iter()
                .map(|byte| format!("{:02X}", byte))
                .collect::<Vec<_>>()
                .join(":"),
        );
        msg.set_ice_pwd(stream.get_local_pwd().to_owned());
        msg.set_ice_ufrag(stream.get_local_ufrag().to_owned());

        // Store ice agent and stream for later use
        self.ice = Some((agent, stream));

        // Prepare to accept the DTLS connection
        let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::dtls()).unwrap();
        acceptor.set_certificate(&self.dtls_cert).unwrap();
        acceptor.set_private_key(&self.dtls_key).unwrap();
        // FIXME: verify remote fingerprint
        self.dtls_srtp_future = Some(DtlsSrtp::handshake(component, acceptor).boxed());

        stream::once(future::ready(Ok(Frame::Client(msg.into()))))
    }

    fn gather_ice_candidates(mut self: Pin<&mut Self>, cx: &mut Context) -> bool {
        if self.candidate_gathering_done {
            return false;
        }
        let stream = match self.ice {
            Some((_, ref mut stream)) => stream,
            None => return false,
        };
        pin_mut!(stream);
        match stream.poll_next(cx) {
            Poll::Ready(Some(mut candidate)) => {
                println!("Local ice candidate: {}", candidate.to_string());

                // Map to public addresses (if configured)
                let config = &self.config;
                match (&mut candidate.address, config.public_v4, config.public_v6) {
                    (webrtc_sdp::address::Address::Ip(IpAddr::V4(addr)), Some(public), _) => {
                        *addr = public;
                    }
                    (webrtc_sdp::address::Address::Ip(IpAddr::V6(addr)), _, Some(public)) => {
                        *addr = public;
                    }
                    _ => {} // non configured
                };

                // Got a new candidate, send it to the client
                let mut msg = msgs::IceCandidate::new();
                msg.set_content(format!("candidate:{}", candidate.to_string()));
                let frame = Frame::Client(msg.into());
                self.stream_to_be_sent = Some(Box::pin(stream::once(future::ready(Ok(frame)))));
                true
            }
            Poll::Ready(None) => {
                self.candidate_gathering_done = true;
                false
            }
            _ => false,
        }
    }

    fn handle_voice_packet(
        &mut self,
        packet: VoicePacket<Clientbound>,
    ) -> impl Stream<Item = Result<Frame, Error>> {
        let (target, session_id, seq_num, opus_data, last_bit) = match packet {
            VoicePacket::Audio {
                target,
                session_id,
                seq_num,
                payload: VoicePacketPayload::Opus(data, last_bit),
                ..
            } => (target, session_id, seq_num, data, last_bit),
            _ => return EitherS::B(stream::empty()),
        };

        // NOTE: the mumble packet id increases by 1 per 10ms of audio contained
        // whereas rtp seq_num should increase by 1 per packet, regardless of audio,
        // but firefox seems to be just fine if we skip over rtp seq_nums.
        // NOTE: we rely on the srtp layer to prevent two-time-pads and by doing so,
        // allow for (reasonable) jitter of incoming voice packets.

        let user = match self.sessions.get_mut(&(session_id as u32)) {
            Some(s) => s,
            None => return EitherS::B(stream::empty()),
        };
        let rtp_ssrc = user.ssrc;

        let mut first_in_transmission = if user.active {
            false
        } else {
            user.start_voice_seq_num = seq_num;
            user.highest_voice_seq_num = seq_num;
            true
        };

        let offset = seq_num - user.start_voice_seq_num;
        let mut rtp_seq_num = user.rtp_seq_num_offset + offset as u32;

        let activity_stream = if last_bit {
            if seq_num <= user.highest_voice_seq_num {
                // Horribly delayed end packet from a previous stream, just drop it
                // (or single packet stream which would be inaudible anyway)
                return EitherS::B(stream::empty());
            }
            // this is the last packet of this voice transmission -> reset counters
            // doing that will effectively trash any delayed packets but that's just
            // a flaw in the mumble protocol and there's nothing we can do about it.
            EitherS::B(user.set_inactive())
        } else {
            EitherS::A(
                if seq_num == user.highest_voice_seq_num && seq_num != user.start_voice_seq_num {
                    // re-transmission, drop it
                    return EitherS::B(stream::empty());
                } else if seq_num >= user.highest_voice_seq_num
                    && seq_num < user.highest_voice_seq_num + 100
                {
                    // probably same voice transmission (also not too far in the future)
                    user.highest_voice_seq_num = seq_num;
                    EitherS::A(user.set_active(target))
                } else if seq_num < user.highest_voice_seq_num
                    && seq_num + 100 > user.highest_voice_seq_num
                {
                    // slightly delayed but probably same voice transmission
                    EitherS::A(user.set_active(target))
                } else {
                    // Either significant jitter (>2s) or we missed the end of the last
                    // transmission. Since >2s jitter will break opus horribly anyway,
                    // we assume the latter and start a new transmission
                    let stream = user.set_inactive();
                    first_in_transmission = true;
                    user.start_voice_seq_num = seq_num;
                    user.highest_voice_seq_num = seq_num;
                    rtp_seq_num = user.rtp_seq_num_offset;
                    EitherS::B(stream.chain(user.set_active(target)))
                },
            )
        };

        let rtp_time = 480 * rtp_seq_num;

        let rtp = RtpPacket {
            header: RtpFixedHeader {
                padding: false,
                marker: first_in_transmission,
                payload_type: 97,
                seq_num: rtp_seq_num as u16,
                timestamp: rtp_time as u32,
                ssrc: rtp_ssrc,
                csrc_list: Vec::new(),
                extension: None,
            },
            payload: opus_data.to_vec(),
            padding: Vec::new(),
        };
        let frame = Frame::Rtp(MuxedPacket::Rtp(rtp));
        EitherS::A(activity_stream.chain(stream::once(future::ready(Ok(frame)))))
    }

    fn process_packet_from_server(
        &mut self,
        packet: ControlPacket<Clientbound>,
    ) -> impl Stream<Item = Result<Frame, Error>> {
        if !self.supports_webrtc() {
            return EitherS::B(stream::once(future::ready(Ok(Frame::Client(packet)))));
        }
        match packet {
            ControlPacket::UDPTunnel(voice) => EitherS::A(self.handle_voice_packet(*voice)),
            ControlPacket::UserState(mut message) => {
                let session_id = message.get_session();
                if !self.sessions.contains_key(&session_id) {
                    let user = self.allocate_ssrc(session_id);
                    message.set_ssrc(user.ssrc);
                }
                EitherS::B(stream::once(future::ready(Ok(Frame::Client(
                    (*message).into(),
                )))))
            }
            ControlPacket::UserRemove(message) => {
                self.free_ssrc(message.get_session());
                EitherS::B(stream::once(future::ready(Ok(Frame::Client(
                    (*message).into(),
                )))))
            }
            other => EitherS::B(stream::once(future::ready(Ok(Frame::Client(other))))),
        }
    }

    fn process_packet_from_client(
        &mut self,
        packet: ControlPacket<Serverbound>,
    ) -> Pin<Box<dyn Stream<Item = Result<Frame, Error>> + Send>> {
        match packet {
            ControlPacket::Authenticate(mut message) => {
                println!("MSG Authenticate: {:?}", message);
                if message.get_webrtc() {
                    // strip webrtc support from the connection (we will be providing it)
                    message.clear_webrtc();
                    // and make sure opus is marked as supported
                    message.set_opus(true);

                    let stream = self.setup_ice();

                    Box::pin(
                        stream::once(future::ready(Ok(Frame::Server((*message).into()))))
                            .chain(stream),
                    )
                } else {
                    Box::pin(stream::once(future::ready(Ok(Frame::Server(
                        (*message).into(),
                    )))))
                }
            }
            ControlPacket::WebRTC(mut message) => {
                println!("Got WebRTC: {:?}", message);
                if let Some((_, stream)) = &mut self.ice {
                    if let (Ok(ufrag), Ok(pwd)) = (
                        CString::new(message.take_ice_ufrag()),
                        CString::new(message.take_ice_pwd()),
                    ) {
                        stream.set_remote_credentials(ufrag, pwd);
                    }
                    // FIXME trigger ICE-restart if required
                    // FIXME store and use remote dtls fingerprint
                }
                Box::pin(stream::empty())
            }
            ControlPacket::IceCandidate(mut message) => {
                let candidate = message.take_content();
                println!("Got ice candidate: {:?}", candidate);
                if let Some((_, stream)) = &mut self.ice {
                    match format!("candidate:{}", candidate).parse() {
                        Ok(SdpAttribute::Candidate(candidate)) => {
                            stream.add_remote_candidate(candidate)
                        }
                        Ok(_) => unreachable!(),
                        Err(err) => {
                            return Box::pin(stream::once(future::ready(Err(io::Error::new(
                                io::ErrorKind::Other,
                                format!("Error parsing ICE candidate: {}", err),
                            )
                            .into()))));
                        }
                    }
                }
                Box::pin(stream::empty())
            }
            ControlPacket::TalkingState(message) => {
                self.target = if message.has_target() {
                    Some(message.get_target() as u8)
                } else {
                    None
                };
                Box::pin(stream::empty())
            }
            other => Box::pin(stream::once(future::ready(Ok(Frame::Server(other))))),
        }
    }

    fn process_rtp_packet(&mut self, buf: &[u8]) -> impl Stream<Item = Result<Frame, Error>> {
        match self.rtp_reader.read_packet(&mut &buf[..]) {
            Ok(MuxedPacket::Rtp(rtp)) => {
                if let Some(target) = self.target {
                    // FIXME derive mumble seq_num from rtp timestamp to properly handle
                    // packet reordering and loss (done). But maybe keep it low?
                    let seq_num = rtp.header.timestamp / 480;

                    let voice_packet = VoicePacket::Audio {
                        _dst: std::marker::PhantomData::<Serverbound>,
                        target,
                        session_id: (),
                        seq_num: seq_num.into(),
                        payload: VoicePacketPayload::Opus(rtp.payload.into(), false),
                        position_info: None,
                    };

                    EitherS::A(stream::once(future::ready(Ok(Frame::Server(
                        voice_packet.into(),
                    )))))
                } else {
                    EitherS::B(stream::empty())
                }
            }
            Ok(MuxedPacket::Rtcp(_rtcp)) => EitherS::B(stream::empty()),
            Err(_err) => EitherS::B(stream::empty()), // FIXME maybe not silently drop the error?
        }
    }
}

impl Future for Connection {
    type Output = Result<(), Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Error>> {
        'poll: loop {
            if let Some((ref mut agent, _)) = self.ice {
                pin_mut!(agent);
                let _ = agent.poll(cx);
            }

            // If there's a frame pending to be sent, sent it before everything else
            if self.next_serverbound_frame.is_some() {
                ready!(self.outbound_server.as_mut().poll_ready(cx)?);
                let frame = self.next_serverbound_frame.take().unwrap();
                self.outbound_server.as_mut().start_send(frame)?;
            }
            if self.next_clientbound_frame.is_some() {
                ready!(self.outbound_client.as_mut().poll_ready(cx)?);
                let frame = self.next_clientbound_frame.take().unwrap();
                self.outbound_client.as_mut().start_send(frame)?;
            }
            if let Some(frame) = self.next_rtp_frame.take() {
                if let Some(ref mut dtls_srtp) = self.dtls_srtp {
                    pin_mut!(dtls_srtp);
                    match dtls_srtp.as_mut().poll_ready(cx)? {
                        Poll::Pending => {
                            self.next_rtp_frame = Some(frame);
                        }
                        Poll::Ready(()) => {
                            dtls_srtp.as_mut().start_send(&frame)?;
                        }
                    }
                } else {
                    // RTP not yet setup, just drop the frame
                    self.next_rtp_frame = None;
                }
            }

            // Send out all pending frames
            if self.stream_to_be_sent.is_some() {
                let mut stream = self.stream_to_be_sent.as_mut().unwrap();
                let stream = stream.deref_mut();
                pin_mut!(stream);
                match ready!(stream.poll_next(cx)) {
                    Some(frame) => {
                        match frame? {
                            Frame::Server(frame) => self.next_serverbound_frame = Some(frame),
                            Frame::Client(frame) => self.next_clientbound_frame = Some(frame),
                            Frame::Rtp(frame) => {
                                let mut buf = Vec::new();
                                self.rtp_writer.write_packet(&mut buf, &frame)?;
                                self.next_rtp_frame = Some(buf)
                            }
                        }
                        continue 'poll;
                    }
                    None => {
                        self.stream_to_be_sent = None;
                    }
                }
            }

            // All frames have been sent (or queued), flush any buffers in the output path
            let _ = self.outbound_client.as_mut().poll_flush(cx)?;
            let _ = self.outbound_server.as_mut().poll_flush(cx)?;
            if let Some(ref mut dtls_srtp) = self.dtls_srtp {
                let _ = Pin::new(dtls_srtp).poll_flush(cx)?;
            }

            // Check/register voice timeouts
            // Note that this must be ran if any new sessions are added or timeouts are
            // modified as otherwise we may be blocking on IO and won't get notified of
            // timeouts. In particular, this means that it has to always be called if
            // we suspect that we may be blocking on inbound IO (outbound is less critical
            // since any action taken as a result of timeouts will have to wait for it
            // anyway), hence this being positioned above the code for incoming packets below.
            // (same applies to the other futures directly below it)
            for session in self.sessions.values_mut() {
                if let Some(timeout) = &mut session.timeout {
                    pin_mut!(timeout);
                    if let Poll::Ready(()) = timeout.poll(cx) {
                        let stream = session.set_inactive();
                        self.stream_to_be_sent = Some(Box::pin(stream));
                        continue 'poll;
                    }
                }
            }

            // Poll ice stream for new candidates
            if self.as_mut().gather_ice_candidates(cx) {
                continue 'poll;
            }

            // Poll dtls_srtp future if required
            if let Some(ref mut future) = self.dtls_srtp_future {
                pin_mut!(future);
                if let Poll::Ready(mut dtls_srtp) = future.poll(cx)? {
                    self.dtls_srtp_future = None;

                    println!("DTLS-SRTP connection established.");

                    dtls_srtp.add_incoming_unknown_ssrcs(self.next_ssrc as usize);
                    dtls_srtp.add_outgoing_unknown_ssrcs(self.next_ssrc as usize);

                    self.dtls_srtp = Some(dtls_srtp);
                }
            }

            // Finally check for incoming packets
            match self.inbound_server.as_mut().poll_next(cx)? {
                Poll::Pending => {}
                Poll::Ready(Some(frame)) => {
                    let stream = self.process_packet_from_server(frame);
                    self.stream_to_be_sent = Some(Box::pin(stream));
                    continue 'poll;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
            }
            match self.inbound_client.as_mut().poll_next(cx)? {
                Poll::Pending => {}
                Poll::Ready(Some(frame)) => {
                    let stream = self.process_packet_from_client(frame);
                    self.stream_to_be_sent = Some(stream);
                    continue 'poll;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
            }
            if let Some(ref mut dtls_srtp) = self.dtls_srtp {
                pin_mut!(dtls_srtp);
                match dtls_srtp.poll_next(cx)? {
                    Poll::Pending => {}
                    Poll::Ready(Some(frame)) => {
                        let stream = self.process_rtp_packet(&frame);
                        self.stream_to_be_sent = Some(Box::pin(stream));
                        continue 'poll;
                    }
                    Poll::Ready(None) => return Poll::Ready(Ok(())),
                }
            }

            return Poll::Pending;
        }
    }
}

#[derive(Clone)]
enum Frame {
    Server(ControlPacket<Serverbound>),
    Client(ControlPacket<Clientbound>),
    Rtp(MuxedPacket<RtpPacket, RtcpCompoundPacket<RtcpPacket>>),
}
