mod ip_packet;
mod tcp;
mod udp;
mod virt_device;

use super::Inbound;
use crate::app::Context;
use crate::app::sniff::Sniffer;
use crate::common::{MAXIMUM_UDP_PAYLOAD_SIZE, invalid_input_error};
use crate::transport::raw::AcceptOpts;
use async_trait::async_trait;
use cfg_if::cfg_if;
use ip_packet::IpPacket;
use ipnet::IpNet;
use smoltcp::wire::{IpProtocol, TcpPacket, UdpPacket};
use std::io::{Error, Result};
use std::mem;
use std::net::{IpAddr, SocketAddr};
#[cfg(unix)]
use std::os::unix::io::RawFd;
use tcp::TcpTun;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{Duration, interval};
use udp::UdpTun;
use virt_device::TokenBuffer;

cfg_if! {
    if #[cfg(any(target_os = "ios",
                 target_os = "macos",
                 target_os = "linux",
                 target_os = "android",
                 target_os = "windows",
                 target_os = "freebsd"))] {
        use tun::{
            create_as_async, AsyncDevice, Configuration as TunConfiguration, AbstractDevice, Error as TunError, Layer,
        };
    } else {
        mod fake_tun;
        use fake_tun::{
            AbstractDevice, AsyncDevice, Configuration as TunConfiguration, Error as TunError, Layer, create_as_async,
        };
    }
}

#[derive(Clone, Debug)]
pub struct TunInbound {
    accept_opts: AcceptOpts,
    tun_config: TunConfiguration,
    address: IpNet,
    sniffer: Sniffer,
    intercept_dns: Option<SocketAddr>,
}

/// TunConfiguration contains a HANDLE, which is a *mut c_void on Windows.
unsafe impl Send for TunInbound {}

impl TunInbound {
    pub fn new(
        name: String,
        address: String,
        destination: String,
        #[cfg(unix)] fd: Option<RawFd>,
        intercept_dns: Option<SocketAddr>,
        accept_opts: AcceptOpts,
        sniffer: Sniffer,
    ) -> Result<Self> {
        let mut tun_config = TunConfiguration::default();
        tun_config.tun_name(name);
        let address: IpNet = address
            .parse()
            .map_err(|_| invalid_input_error(format!("invalid tun address of {}", address)))?;
        tun_config
            .address(address.addr())
            .netmask(address.netmask());
        let destination: IpAddr = destination.parse().map_err(|_| {
            invalid_input_error(format!("invalid tun destination of {}", destination))
        })?;
        tun_config.destination(destination);

        #[cfg(unix)]
        if let Some(fd) = fd {
            tun_config.raw_fd(fd);
            tun_config.close_fd_on_drop(false);
        }

        tun_config.layer(Layer::L3).up();

        // XXX: tun2 set IFF_NO_PI by default.
        //
        // #[cfg(target_os = "linux")]
        // self.tun_config.platform_config(|tun_config| {
        //     // IFF_NO_PI preventing excessive buffer reallocating
        //     tun_config.packet_information(false);
        // });

        Ok(Self {
            accept_opts,
            address,
            tun_config,
            sniffer,
            intercept_dns,
        })
    }
}

#[async_trait]
impl Inbound for TunInbound {
    fn clone_box(&self) -> Box<dyn Inbound> {
        Box::new(self.clone())
    }

    async fn run(&self, context: Context, channel: Option<mpsc::Sender<String>>) -> Result<()> {
        let device = match create_as_async(&self.tun_config) {
            Ok(d) => d,
            Err(TunError::Io(err)) => return Err(err),
            Err(err) => return Err(Error::other(err)),
        };
        log::info!(
            "Creating tun interface with ip {}/{}",
            self.address.addr(),
            self.address.prefix_len()
        );
        if let Some(channel) = channel {
            let _ = channel.send("tun".to_string()).await;
        }

        let tcp = TcpTun::new(
            context.clone(),
            self.accept_opts.clone(),
            self.sniffer.clone(),
            device.mtu().unwrap_or(1500) as u32,
        );

        let (udp, udp_cleanup_interval, udp_keepalive_rx) =
            UdpTun::new(context, self.intercept_dns);

        let handler = TunHandler {
            device,
            address: self.address,
            tcp,
            udp,
            udp_cleanup_interval,
            udp_keepalive_rx,
        };

        handler.run().await
    }
}

struct TunHandler {
    device: AsyncDevice,
    address: IpNet,
    tcp: TcpTun,
    udp: UdpTun,
    udp_cleanup_interval: Duration,
    udp_keepalive_rx: mpsc::Receiver<SocketAddr>,
}

impl TunHandler {
    pub async fn run(mut self) -> Result<()> {
        let address_broadcast = self.address.broadcast();

        let create_packet_buffer = || {
            let mut packet_buffer = TokenBuffer::with_capacity(MAXIMUM_UDP_PAYLOAD_SIZE);
            unsafe {
                packet_buffer.set_len(MAXIMUM_UDP_PAYLOAD_SIZE);
            }
            packet_buffer
        };

        let mut packet_buffer = create_packet_buffer();
        let mut udp_cleanup_timer = interval(self.udp_cleanup_interval);

        loop {
            tokio::select! {
                // tun device
                n = self.device.read(&mut packet_buffer) => {
                    let n = n?;

                    let mut packet_buffer = mem::replace(&mut packet_buffer, create_packet_buffer());
                    unsafe {
                        packet_buffer.set_len(n);
                    }

                    log::trace!("[TUN] received IP packet with length {}", packet_buffer.len());

                    if let Err(err) = self.handle_tun_frame(&address_broadcast, packet_buffer).await {
                        log::error!("[TUN] handle IP frame failed, error: {}", err);
                    }
                }

                // TCP channel sent back
                packet = self.tcp.recv_packet() => {
                    match self.device.write(&packet).await {
                        Ok(n) => {
                            if n < packet.len() {
                                log::warn!("[TUN] sent IP packet (TCP), but truncated. sent {} < {}", n, packet.len());
                            } else {
                                log::trace!("[TUN] sent IP packet (TCP) length {}", packet.len());
                            }
                        }
                        Err(err) => {
                            log::error!("[TUN] failed to set packet information, error: {}", err);
                        }
                    }
                }

                // UDP channel sent back
                packet = self.udp.recv_packet() => {
                    match self.device.write(&packet).await {
                        Ok(n) => {
                            if n < packet.len() {
                                log::warn!("[TUN] sent IP packet (UDP), but truncated. sent {} < {}", n, packet.len());
                            } else {
                                log::trace!("[TUN] sent IP packet (UDP) length {:?}", packet.len());
                            }
                        }
                        Err(err) => {
                            log::error!("[TUN] failed to set packet information, error: {}", err);
                        }
                    }
                }

                // UDP cleanup expired associations
                _ = udp_cleanup_timer.tick() => {
                    self.udp.cleanup_expired().await;
                }

                // UDP keep-alive associations
                peer_addr_opt = self.udp_keepalive_rx.recv() => {
                    let peer_addr = peer_addr_opt.expect("UDP keep-alive channel closed unexpectly");
                    self.udp.keep_alive(&peer_addr).await;
                }
            }
        }
    }

    async fn handle_tun_frame(
        &mut self,
        device_broadcast_addr: &IpAddr,
        frame: TokenBuffer,
    ) -> smoltcp::wire::Result<()> {
        let packet = match IpPacket::new_checked(frame.as_ref())? {
            Some(packet) => packet,
            None => {
                log::warn!("unrecognized IP packet with length {}", frame.len());
                return Ok(());
            }
        };

        let src_ip_addr = packet.src_addr();
        let dst_ip_addr = packet.dst_addr();
        let src_non_unicast = src_ip_addr == *device_broadcast_addr
            || match src_ip_addr {
                IpAddr::V4(v4) => v4.is_broadcast() || v4.is_multicast() || v4.is_unspecified(),
                IpAddr::V6(v6) => v6.is_multicast() || v6.is_unspecified(),
            };
        let dst_non_unicast = dst_ip_addr == *device_broadcast_addr
            || match dst_ip_addr {
                IpAddr::V4(v4) => v4.is_broadcast() || v4.is_multicast() || v4.is_unspecified(),
                IpAddr::V6(v6) => v6.is_multicast() || v6.is_unspecified(),
            };

        if src_non_unicast || dst_non_unicast {
            log::trace!(
                "[TUN] IP packet {} (unicast? {}) -> {} (unicast? {}) throwing away",
                src_ip_addr,
                !src_non_unicast,
                dst_ip_addr,
                !dst_non_unicast
            );
            return Ok(());
        }

        match packet.protocol() {
            IpProtocol::Tcp => {
                let tcp_packet = match TcpPacket::new_checked(packet.payload()) {
                    Ok(p) => p,
                    Err(err) => {
                        log::error!(
                            "invalid TCP packet err: {}, src_ip: {}, dst_ip: {}, payload length: {}",
                            err,
                            packet.src_addr(),
                            packet.dst_addr(),
                            packet.payload().len()
                        );
                        return Ok(());
                    }
                };

                let src_port = tcp_packet.src_port();
                let dst_port = tcp_packet.dst_port();

                let src_addr = SocketAddr::new(packet.src_addr(), src_port);
                let dst_addr = SocketAddr::new(packet.dst_addr(), dst_port);

                log::trace!(
                    "[TUN] TCP packet {} (unicast? {}) -> {} (unicast? {}) {}",
                    src_addr,
                    !src_non_unicast,
                    dst_addr,
                    !dst_non_unicast,
                    tcp_packet
                );

                // TCP first handshake packet.
                if let Err(err) = self
                    .tcp
                    .handle_packet(src_addr, dst_addr, &tcp_packet)
                    .await
                {
                    log::error!(
                        "handle TCP packet failed, error: {}, {} <-> {}, packet: {:?}",
                        err,
                        src_addr,
                        dst_addr,
                        tcp_packet
                    );
                }

                self.tcp.drive_interface_state(frame).await;
            }
            IpProtocol::Udp => {
                let udp_packet = match UdpPacket::new_checked(packet.payload()) {
                    Ok(p) => p,
                    Err(err) => {
                        log::error!(
                            "invalid UDP packet err: {}, src_ip: {}, dst_ip: {}, payload length: {}",
                            err,
                            packet.src_addr(),
                            packet.dst_addr(),
                            packet.payload().len(),
                        );
                        return Ok(());
                    }
                };

                let src_port = udp_packet.src_port();
                let dst_port = udp_packet.dst_port();

                let src_addr = SocketAddr::new(src_ip_addr, src_port);
                let dst_addr = SocketAddr::new(packet.dst_addr(), dst_port);

                let payload = udp_packet.payload();
                log::trace!(
                    "[TUN] UDP packet {} (unicast? {}) -> {} (unicast? {}) {}",
                    src_addr,
                    !src_non_unicast,
                    dst_addr,
                    !dst_non_unicast,
                    udp_packet
                );

                if let Err(err) = self.udp.handle_packet(src_addr, dst_addr, payload).await {
                    log::error!(
                        "handle UDP packet failed, err: {}, packet: {:?}",
                        err,
                        udp_packet
                    );
                }
            }
            IpProtocol::Icmp | IpProtocol::Icmpv6 => {
                // ICMP is handled by TCP's Interface.
                // smoltcp's interface will always send replies to EchoRequest
                self.tcp.drive_interface_state(frame).await;
            }
            _ => {
                log::debug!("IP packet ignored (protocol: {:?})", packet.protocol());
                return Ok(());
            }
        }

        Ok(())
    }
}
