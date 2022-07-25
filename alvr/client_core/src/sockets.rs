use alvr_common::{parking_lot::Mutex, prelude::*, RelaxedAtomic, ALVR_NAME};
use alvr_sockets::{
    LdcTcpReceiver, LdcTcpSender, RequestPacket, ResponsePacket, SsePacket, CONTROL_PORT,
    HANDSHAKE_PACKET_SIZE_BYTES, HANDSHAKE_STREAM, KEEPALIVE_INTERVAL, LOCAL_IP, REQUEST_STREAM,
    EVENT_STREAM,
};
use futures::stream::Scan;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    io::ErrorKind,
    marker::PhantomData,
    net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket},
    sync::{
        mpsc::{self, RecvError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

pub struct AnnouncerSocket {
    socket: UdpSocket,
    packet: [u8; 56],
}

impl AnnouncerSocket {
    pub fn new(hostname: &str) -> StrResult<Self> {
        let socket = UdpSocket::bind((LOCAL_IP, CONTROL_PORT)).map_err(err!())?;
        socket.set_broadcast(true).map_err(err!())?;

        let mut packet = [0; 56];
        packet[0..ALVR_NAME.len()].copy_from_slice(ALVR_NAME.as_bytes());
        packet[16..24].copy_from_slice(&alvr_common::protocol_id().to_le_bytes());
        packet[24..24 + hostname.len()].copy_from_slice(hostname.as_bytes());

        Ok(Self { socket, packet })
    }

    pub fn broadcast(&self) -> StrResult {
        self.socket
            .send_to(&self.packet, (Ipv4Addr::BROADCAST, CONTROL_PORT))
            .map_err(err!())?;
        Ok(())
    }
}

pub struct RequestSocket {
    send_socket: Arc<Mutex<LdcTcpSender>>,
    response_receiver: mpsc::Receiver<Vec<u8>>,
    last_socket_send: Arc<Mutex<Instant>>,
}

impl RequestSocket {
    pub fn request(&mut self, message: RequestPacket) -> IntResult<ResponsePacket> {
        let buffer = bincode::serialize(&message).map_err(to_int_e!())?;

        *self.last_socket_send.lock() = Instant::now();
        self.send_socket
            .lock()
            .send(REQUEST_STREAM, &buffer)
            .map_err(int_e!())?;

        match self.response_receiver.recv() {
            Ok(buffer) => bincode::deserialize(&buffer).map_err(to_int_e!()),
            Err(RecvError) => interrupt(),
        }
    }
}

pub struct SseSocket {
    receiver: mpsc::Receiver<Vec<u8>>,
}

impl SseSocket {
    pub fn recv(&mut self) -> IntResult<SsePacket> {
        match self.receiver.recv() {
            Ok(buffer) => bincode::deserialize(&buffer).map_err(to_int_e!()),
            Err(RecvError) => interrupt(),
        }
    }
}

pub enum ScanResult {
    Connected {
        server_ip: IpAddr,
        request_socket: RequestSocket,
        sse_socket: SseSocket,
    },
    UnrelatedPeer,
    MismatchedVersion,
}

pub struct ControlListenerSocket {
    running: Arc<RelaxedAtomic>,
    inner: TcpListener,
}

impl ControlListenerSocket {
    pub fn new(running: Arc<RelaxedAtomic>) -> StrResult<Self> {
        let inner = TcpListener::bind((LOCAL_IP, CONTROL_PORT)).map_err(err!())?;
        inner.set_nonblocking(true).map_err(err!())?;

        Ok(Self { running, inner })
    }

    pub fn scan(&self) -> IntResult<ScanResult> {
        let (socket, server_address) = match self.inner.accept() {
            Ok(pair) => pair,
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted {
                    return interrupt();
                } else {
                    return int_fmt_e!("{e}");
                }
            }
        };

        socket.set_nonblocking(true).map_err(to_int_e!())?; // check if necessary

        let send_socket = Arc::new(Mutex::new(LdcTcpSender::new(
            socket.try_clone().map_err(to_int_e!())?,
            Arc::clone(&self.running),
        )));
        let mut receive_socket = LdcTcpReceiver::new(socket, Arc::clone(&self.running));

        // Check server compatibility
        let (_, packet) = receive_socket.recv().map_err(int_e!())?;

        if &packet[0..ALVR_NAME.len()] != ALVR_NAME.as_bytes()
            || !packet[ALVR_NAME.len()..16].iter().all(|b| *b == 0)
        {
            return Ok(ScanResult::UnrelatedPeer);
        } else if packet[16..24] != alvr_common::protocol_id().to_le_bytes() {
            return Ok(ScanResult::MismatchedVersion);
        }

        let (response_sender, response_receiver) = mpsc::sync_channel(0);
        let (sse_sender, sse_receiver) = mpsc::sync_channel(0);

        let last_socket_send = Arc::new(Mutex::new(Instant::now()));

        let request_socket = RequestSocket {
            send_socket: Arc::clone(&send_socket),
            response_receiver,
            last_socket_send: Arc::clone(&last_socket_send),
        };
        let sse_socket = SseSocket {
            receiver: sse_receiver,
        };

        // Keepalive
        thread::spawn(move || -> IntResult {
            loop {
                thread::sleep(KEEPALIVE_INTERVAL);

                let mut last_socket_send = last_socket_send.lock();
                let now = Instant::now();
                if now - *last_socket_send > KEEPALIVE_INTERVAL {
                    *last_socket_send = now;
                    send_socket.lock().send(HANDSHAKE_STREAM, &[])?;
                }
            }
        });

        // The Poller polls packets for both requests and sse. Having a separate entity for polling
        // packets prevents stalling when making requests as a response of a sse.
        thread::spawn(move || -> IntResult {
            loop {
                let (stream_id, buffer) = receive_socket.recv().map_err(int_e!())?;

                if stream_id == REQUEST_STREAM {
                    response_sender.send(buffer).map_err(to_int_e!())?;
                } else if stream_id == EVENT_STREAM {
                    sse_sender.send(buffer).map_err(to_int_e!())?;
                }
                // else: it may be a keepalive packet
            }
        });

        Ok(ScanResult::Connected {
            server_ip: server_address.ip(),
            request_socket,
            sse_socket,
        })
    }
}
