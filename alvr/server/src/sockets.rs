use alvr_common::{parking_lot::Mutex, prelude::*, RelaxedAtomic, ALVR_NAME};
use alvr_events::EventType;
use alvr_sockets::{
    LdcTcpReceiver, LdcTcpSender, RequestPacket, ResponsePacket, SsePacket, CONTROL_PORT,
    HANDSHAKE_PACKET_SIZE_BYTES, HANDSHAKE_STREAM, KEEPALIVE_INTERVAL, LOCAL_IP, REQUEST_STREAM,
    EVENT_STREAM,
};
use std::{
    io::ErrorKind,
    net::{IpAddr, TcpStream, UdpSocket},
    sync::Arc,
    thread,
    time::Instant,
};

pub struct WelcomeSocket {
    socket: UdpSocket,
    buffer: [u8; HANDSHAKE_PACKET_SIZE_BYTES],
}

impl WelcomeSocket {
    pub fn new() -> StrResult<Self> {
        let socket = UdpSocket::bind((LOCAL_IP, CONTROL_PORT)).map_err(err!())?;
        socket.set_nonblocking(true).map_err(err!())?;

        Ok(Self {
            socket,
            buffer: [0; HANDSHAKE_PACKET_SIZE_BYTES],
        })
    }

    // Returns: client IP, client hostname
    pub fn recv_non_blocking(&mut self) -> IntResult<(String, IpAddr)> {
        let (size, address) = match self.socket.recv_from(&mut self.buffer) {
            Ok(pair) => pair,
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted {
                    return interrupt();
                } else {
                    return int_fmt_e!("{e}");
                }
            }
        };

        if size == HANDSHAKE_PACKET_SIZE_BYTES
            && &self.buffer[..ALVR_NAME.len()] == ALVR_NAME.as_bytes()
            && self.buffer[ALVR_NAME.len()..16].iter().all(|b| *b == 0)
        {
            let mut protocol_id_bytes = [0; 8];
            protocol_id_bytes.copy_from_slice(&self.buffer[16..24]);
            let received_protocol_id = u64::from_le_bytes(protocol_id_bytes);

            if received_protocol_id != alvr_common::protocol_id() {
                alvr_events::send_event(EventType::ClientFoundWrongVersion(format!(
                    "Expected protocol ID {}, Found {received_protocol_id}",
                    alvr_common::protocol_id()
                )));
            }

            let mut hostname_bytes = [0; 32];
            hostname_bytes.copy_from_slice(&self.buffer[24..56]);
            let hostname = std::str::from_utf8(&hostname_bytes)
                .map_err(to_int_e!())?
                .trim_end_matches('\x00')
                .to_owned();

            Ok((hostname, address.ip()))
        } else if &self.buffer[..16] == b"\x00\x00\x00\x00\x04\x00\x00\x00\x00\x00\x00\x00ALVR" {
            alvr_events::send_event(EventType::ClientFoundWrongVersion("v14 to v19".into()));

            interrupt()
        } else if &self.buffer[..5] == b"\x01ALVR" {
            // People might still download the client from the polygraphene reposiory
            alvr_events::send_event(EventType::ClientFoundWrongVersion("v11 or previous".into()));

            interrupt()
        } else {
            // Unexpected packet.
            // Note: no need to check for v12 and v13, not found in the wild anymore
            interrupt()
        }
    }
}

pub struct ResponseSocket {
    receive_socket: LdcTcpReceiver,
    send_socket: Arc<Mutex<LdcTcpSender>>,
    last_socket_send: Arc<Mutex<Instant>>,
}

impl ResponseSocket {
    // Poll requests and send responses. The callback needs to return as soon as possible and events
    // cannpt be generated within; instead spawn a new thread if necessary
    pub fn poll(&mut self, callback: impl FnOnce(RequestPacket) -> ResponsePacket) -> IntResult {
        let (stream_id, buffer) = self.receive_socket.recv().map_err(int_e!())?;

        debug_assert_eq!(stream_id, REQUEST_STREAM);

        let request = bincode::deserialize(&buffer).map_err(to_int_e!())?;
        let response = callback(request);
        self.receive_socket.push_buffer(stream_id, buffer);

        let buffer = bincode::serialize(&response).map_err(to_int_e!())?;
        *self.last_socket_send.lock() = Instant::now();
        self.send_socket
            .lock()
            .send(REQUEST_STREAM, &buffer)
            .map_err(int_e!())
    }
}

pub struct SseSocket {
    send_socket: Arc<Mutex<LdcTcpSender>>,
    last_socket_send: Arc<Mutex<Instant>>,
}

impl SseSocket {
    pub fn send(&mut self, event: SsePacket) -> IntResult {
        let buffer = bincode::serialize(&event).map_err(to_int_e!())?;
        *self.last_socket_send.lock() = Instant::now();
        self.send_socket.lock().send(EVENT_STREAM, &buffer)
    }
}

// Try to connect to any client. If returns None if all fail.
pub fn split_server_control_socket<'a>(
    client_ips: impl Iterator<Item = &'a IpAddr>,
    running: Arc<RelaxedAtomic>,
) -> IntResult<(IpAddr, ResponseSocket, SseSocket)> {
    let client_addresses = client_ips
        .map(|&ip| (ip, CONTROL_PORT).into())
        .collect::<Vec<_>>();

    let socket = loop {
        match TcpStream::connect(client_addresses.as_slice()) {
            Ok(socket) => break socket,
            Err(e) => {
                if e.kind() != ErrorKind::WouldBlock && e.kind() != ErrorKind::Interrupted {
                    return int_fmt_e!("{e}");
                }
            }
        }
    };

    socket.set_nonblocking(true).map_err(to_int_e!())?;

    let client_address = socket.peer_addr().map_err(to_int_e!())?;

    let mut send_socket = LdcTcpSender::new(
        socket.try_clone().map_err(to_int_e!())?,
        Arc::clone(&running),
    );
    let receive_socket = LdcTcpReceiver::new(socket, Arc::clone(&running));

    // Send server handshake packet
    let mut buffer = [0; 24];
    buffer[0..ALVR_NAME.len()].copy_from_slice(ALVR_NAME.as_bytes());
    buffer[16..24].copy_from_slice(&alvr_common::protocol_id().to_le_bytes());
    send_socket
        .send(HANDSHAKE_STREAM, &buffer)
        .map_err(int_e!())?;

    let send_socket = Arc::new(Mutex::new(send_socket));
    let last_socket_send = Arc::new(Mutex::new(Instant::now()));

    let response_socket = ResponseSocket {
        send_socket: Arc::clone(&send_socket),
        receive_socket,
        last_socket_send: Arc::clone(&last_socket_send),
    };
    let sse_socket = SseSocket {
        send_socket: Arc::clone(&send_socket),
        last_socket_send: Arc::clone(&last_socket_send),
    };

    // Keepalive
    thread::spawn(move || -> IntResult {
        loop {
            // wait at the start instead of end of scope: make sure last_socket_send is locked for
            // as little as possible
            thread::sleep(KEEPALIVE_INTERVAL);

            let mut last_socket_send = last_socket_send.lock();
            let now = Instant::now();
            if now - *last_socket_send > KEEPALIVE_INTERVAL {
                *last_socket_send = now;
                send_socket.lock().send(HANDSHAKE_STREAM, &[])?;
            }
        }
    });

    Ok((client_address.ip(), response_socket, sse_socket))
}
