mod ldc_tcp_socket;
mod packets;

use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

pub use ldc_tcp_socket::*;
pub use packets::*;

pub const LOCAL_IP: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
pub const CONTROL_PORT: u16 = 9943;
pub const HANDSHAKE_PACKET_SIZE_BYTES: usize = 56; // this may change in future protocols
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
