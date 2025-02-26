use crate::tracing::error::{TraceResult, TracerError};
use crate::tracing::net::platform::Socket;
use crate::tracing::net::socket::TracerSocket as _;
use crate::tracing::net::{ipv4, ipv6, platform, Network};
use crate::tracing::probe::ProbeResponse;
use crate::tracing::types::{PacketSize, PayloadPattern, Sequence, TraceId, TypeOfService};
use crate::tracing::{
    MultipathStrategy, PortDirection, Probe, TracerChannelConfig, TracerProtocol,
};
use arrayvec::ArrayVec;
use itertools::Itertools;
use std::net::IpAddr;
use std::time::{Duration, SystemTime};

/// The maximum size of the IP packet we allow.
pub const MAX_PACKET_SIZE: usize = 1024;

/// The maximum number of TCP probes we allow.
const MAX_TCP_PROBES: usize = 256;

/// A channel for sending and receiving `Probe` packets.
pub struct TracerChannel {
    protocol: TracerProtocol,
    src_addr: IpAddr,
    ipv4_length_order: platform::PlatformIpv4FieldByteOrder,
    dest_addr: IpAddr,
    identifier: TraceId,
    packet_size: PacketSize,
    payload_pattern: PayloadPattern,
    tos: TypeOfService,
    initial_sequence: Sequence,
    multipath_strategy: MultipathStrategy,
    port_direction: PortDirection,
    read_timeout: Duration,
    tcp_connect_timeout: Duration,
    icmp_send_socket: Socket,
    udp_send_socket: Socket,
    recv_socket: Socket,
    tcp_probes: ArrayVec<TcpProbe, MAX_TCP_PROBES>,
}

impl TracerChannel {
    /// Create an `IcmpChannel`.
    ///
    /// This operation requires the `CAP_NET_RAW` capability on Linux.
    pub fn connect(config: &TracerChannelConfig) -> TraceResult<Self> {
        if usize::from(config.packet_size.0) > MAX_PACKET_SIZE {
            return Err(TracerError::InvalidPacketSize(usize::from(
                config.packet_size.0,
            )));
        }
        platform::startup()?;
        let ipv4_length_order =
            platform::PlatformIpv4FieldByteOrder::for_address(config.source_addr)?;
        let icmp_send_socket = make_icmp_send_socket(config.source_addr)?;
        let udp_send_socket = make_udp_send_socket(config.source_addr)?;
        let recv_socket = make_recv_socket(config.source_addr)?;
        Ok(Self {
            protocol: config.protocol,
            src_addr: config.source_addr,
            ipv4_length_order,
            dest_addr: config.target_addr,
            identifier: config.identifier,
            packet_size: config.packet_size,
            payload_pattern: config.payload_pattern,
            tos: config.tos,
            initial_sequence: config.initial_sequence,
            multipath_strategy: config.multipath_strategy,
            port_direction: config.port_direction,
            read_timeout: config.read_timeout,
            tcp_connect_timeout: config.tcp_connect_timeout,
            icmp_send_socket,
            udp_send_socket,
            recv_socket,
            tcp_probes: ArrayVec::new(),
        })
    }
}

impl Network for TracerChannel {
    fn send_probe(&mut self, probe: Probe) -> TraceResult<()> {
        match self.protocol {
            TracerProtocol::Icmp => self.dispatch_icmp_probe(probe),
            TracerProtocol::Udp => self.dispatch_udp_probe(probe),
            TracerProtocol::Tcp => self.dispatch_tcp_probe(probe),
        }
    }

    fn recv_probe(&mut self) -> TraceResult<Option<ProbeResponse>> {
        match self.protocol {
            TracerProtocol::Icmp | TracerProtocol::Udp => self.recv_icmp_probe(),
            TracerProtocol::Tcp => match self.recv_tcp_sockets()? {
                None => self.recv_icmp_probe(),
                resp => Ok(resp),
            },
        }
    }
}

impl TracerChannel {
    /// Dispatch a ICMP probe.
    fn dispatch_icmp_probe(&mut self, probe: Probe) -> TraceResult<()> {
        match (self.src_addr, self.dest_addr) {
            (IpAddr::V4(src_addr), IpAddr::V4(dest_addr)) => ipv4::dispatch_icmp_probe(
                &mut self.icmp_send_socket,
                probe,
                src_addr,
                dest_addr,
                self.identifier,
                self.packet_size,
                self.payload_pattern,
                self.ipv4_length_order,
            ),
            (IpAddr::V6(src_addr), IpAddr::V6(dest_addr)) => ipv6::dispatch_icmp_probe(
                &mut self.icmp_send_socket,
                probe,
                src_addr,
                dest_addr,
                self.identifier,
                self.packet_size,
                self.payload_pattern,
            ),
            _ => unreachable!(),
        }
    }

    /// Dispatch a UDP probe.
    fn dispatch_udp_probe(&mut self, probe: Probe) -> TraceResult<()> {
        match (self.src_addr, self.dest_addr) {
            (IpAddr::V4(src_addr), IpAddr::V4(dest_addr)) => ipv4::dispatch_udp_probe(
                &mut self.udp_send_socket,
                probe,
                src_addr,
                dest_addr,
                self.initial_sequence,
                self.multipath_strategy,
                self.port_direction,
                self.packet_size,
                self.payload_pattern,
                self.ipv4_length_order,
            ),
            (IpAddr::V6(src_addr), IpAddr::V6(dest_addr)) => ipv6::dispatch_udp_probe(
                &mut self.udp_send_socket,
                probe,
                src_addr,
                dest_addr,
                self.port_direction,
                self.packet_size,
                self.payload_pattern,
            ),
            _ => unreachable!(),
        }
    }

    /// Dispatch a TCP probe.
    fn dispatch_tcp_probe(&mut self, probe: Probe) -> TraceResult<()> {
        let socket = match (self.src_addr, self.dest_addr) {
            (IpAddr::V4(src_addr), IpAddr::V4(dest_addr)) => {
                ipv4::dispatch_tcp_probe(probe, src_addr, dest_addr, self.port_direction, self.tos)
            }
            (IpAddr::V6(src_addr), IpAddr::V6(dest_addr)) => {
                ipv6::dispatch_tcp_probe(probe, src_addr, dest_addr, self.port_direction)
            }
            _ => unreachable!(),
        }?;
        self.tcp_probes
            .push(TcpProbe::new(socket, probe.sequence, SystemTime::now()));
        Ok(())
    }

    /// Generate a `ProbeResponse` for the next available ICMP packet, if any
    fn recv_icmp_probe(&mut self) -> TraceResult<Option<ProbeResponse>> {
        if self.recv_socket.is_readable(self.read_timeout)? {
            match self.dest_addr {
                IpAddr::V4(_) => ipv4::recv_icmp_probe(
                    &mut self.recv_socket,
                    self.protocol,
                    self.multipath_strategy,
                    self.port_direction,
                ),
                IpAddr::V6(_) => {
                    ipv6::recv_icmp_probe(&mut self.recv_socket, self.protocol, self.port_direction)
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Generate synthetic `ProbeResponse` if a TCP socket is connected or if the connection was refused.
    ///
    /// Any TCP socket which has not connected or failed after a timeout will be removed.
    fn recv_tcp_sockets(&mut self) -> TraceResult<Option<ProbeResponse>> {
        self.tcp_probes
            .retain(|probe| probe.start.elapsed().unwrap_or_default() < self.tcp_connect_timeout);
        let found_index = self
            .tcp_probes
            .iter()
            .find_position(|&probe| probe.socket.is_writable().unwrap_or_default())
            .map(|(i, _)| i);
        if let Some(i) = found_index {
            let probe = self.tcp_probes.remove(i);
            match self.dest_addr {
                IpAddr::V4(_) => {
                    ipv4::recv_tcp_socket(&probe.socket, probe.sequence, self.dest_addr)
                }
                IpAddr::V6(_) => {
                    ipv6::recv_tcp_socket(&probe.socket, probe.sequence, self.dest_addr)
                }
            }
        } else {
            Ok(None)
        }
    }
}

/// An entry in the TCP probes array.
struct TcpProbe {
    socket: Socket,
    sequence: Sequence,
    start: SystemTime,
}

impl TcpProbe {
    pub fn new(socket: Socket, sequence: Sequence, start: SystemTime) -> Self {
        Self {
            socket,
            sequence,
            start,
        }
    }
}

/// Make a socket for sending raw `ICMP` packets.
fn make_icmp_send_socket(addr: IpAddr) -> TraceResult<Socket> {
    Ok(match addr {
        IpAddr::V4(_) => Socket::new_icmp_send_socket_ipv4(),
        IpAddr::V6(_) => Socket::new_icmp_send_socket_ipv6(),
    }?)
}

/// Make a socket for sending `UDP` packets.
fn make_udp_send_socket(addr: IpAddr) -> TraceResult<Socket> {
    Ok(match addr {
        IpAddr::V4(_) => Socket::new_udp_send_socket_ipv4(),
        IpAddr::V6(_) => Socket::new_udp_send_socket_ipv6(),
    }?)
}

/// Make a socket for receiving raw `ICMP` packets.
fn make_recv_socket(addr: IpAddr) -> TraceResult<Socket> {
    Ok(match addr {
        IpAddr::V4(ipv4addr) => Socket::new_recv_socket_ipv4(ipv4addr),
        IpAddr::V6(ipv6addr) => Socket::new_recv_socket_ipv6(ipv6addr),
    }?)
}
