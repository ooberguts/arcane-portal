use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use if_addrs::Ifv4Addr;
use mdns_sd::{IfKind, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use socket2::{Domain, Protocol, Socket, Type};

use crate::network::same_lan;

const SERVICE_TYPE: &str = "_arcane-join._tcp.local.";
const RETURN_MAGIC: [u8; 4] = *b"APR1";
const OFFER_TIMEOUT: Duration = Duration::from_secs(7);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1_400);
const TOKEN_TIMEOUT: Duration = Duration::from_millis(400);
const MAX_INTERFACES: usize = 8;
const MAX_RECENT_OFFERS: usize = 32;

/// A live broadcaster can initiate the TCP connection when direct inbound TCP is blocked.
pub(crate) fn watch(
    daemon: &ServiceDaemon,
    room_id: String,
    interfaces: Vec<Ifv4Addr>,
    on_stream: impl Fn(TcpStream) + Send + 'static,
) -> io::Result<()> {
    let events = daemon.browse(SERVICE_TYPE).map_err(io::Error::other)?;
    thread::Builder::new()
        .name("arcane-return-connect".into())
        .spawn(move || {
            let mut recent = VecDeque::new();
            while let Ok(event) = events.recv() {
                let ServiceEvent::ServiceResolved(service) = event else {
                    continue;
                };
                let Some((token, endpoints)) = offer(&service, &room_id, &interfaces) else {
                    continue;
                };
                if recent.contains(&token) {
                    continue;
                }
                if recent.len() == MAX_RECENT_OFFERS {
                    recent.pop_front();
                }
                recent.push_back(token);
                if let Some(stream) = endpoints
                    .into_iter()
                    .find_map(|(destination, source)| connect(destination, source, &token).ok())
                {
                    on_stream(stream);
                }
            }
        })?;
    Ok(())
}

type Endpoints = Vec<(SocketAddrV4, Ipv4Addr)>;

fn offer(
    service: &ResolvedService,
    room_id: &str,
    interfaces: &[Ifv4Addr],
) -> Option<([u8; 16], Endpoints)> {
    if service.get_property_val_str("room")? != room_id || service.get_port() == 0 {
        return None;
    }
    let token = parse_token(service.get_property_val_str("token")?)?;
    let mut candidates: Vec<_> = service
        .get_addresses_v4()
        .into_iter()
        .filter_map(|address| {
            interfaces
                .iter()
                .filter(|interface| same_lan(interface, address))
                .max_by_key(|interface| interface.prefixlen)
                .map(|interface| (address, interface))
        })
        .collect();
    candidates.sort_by_key(|(address, interface)| {
        (
            address.is_link_local(),
            std::cmp::Reverse(interface.prefixlen),
            *address,
        )
    });
    let endpoints: Endpoints = candidates
        .into_iter()
        .take(3)
        .map(|(address, interface)| (SocketAddrV4::new(address, service.get_port()), interface.ip))
        .collect();
    (!endpoints.is_empty()).then_some((token, endpoints))
}

fn connect(destination: SocketAddrV4, source: Ipv4Addr, token: &[u8; 16]) -> io::Result<TcpStream> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.bind(&SocketAddrV4::new(source, 0).into())?;
    socket.connect_timeout(&destination.into(), CONNECT_TIMEOUT)?;
    let mut stream = TcpStream::from(socket);
    stream.set_read_timeout(Some(TOKEN_TIMEOUT))?;
    stream.set_write_timeout(Some(TOKEN_TIMEOUT))?;
    let mut proof = [0; 20];
    stream.read_exact(&mut proof)?;
    if proof[..4] != RETURN_MAGIC || proof[4..] != *token {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid return connection",
        ));
    }
    stream.write_all(token)?;
    Ok(stream)
}

pub(crate) fn receive(
    room_id: &str,
    expected_hosts: &[SocketAddrV4],
    interfaces: &[Ifv4Addr],
    cancelled: impl Fn() -> bool,
) -> io::Result<TcpStream> {
    if cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let mut listeners = Vec::new();
    let mut addresses = Vec::new();
    let mut port = 0;
    for interface in interfaces.iter().filter(|interface| {
        expected_hosts
            .iter()
            .any(|host| same_lan(interface, *host.ip()))
    }) {
        if addresses.contains(&IpAddr::V4(interface.ip)) {
            continue;
        }
        if let Ok(listener) = TcpListener::bind((interface.ip, port)) {
            listener.set_nonblocking(true)?;
            port = listener.local_addr()?.port();
            listeners.push(listener);
            addresses.push(IpAddr::V4(interface.ip));
            if listeners.len() == MAX_INTERFACES {
                break;
            }
        }
    }
    if listeners.is_empty() {
        return Err(io::ErrorKind::AddrNotAvailable.into());
    }

    let mut token = [0u8; 16];
    getrandom::fill(&mut token).map_err(|error| io::Error::other(error.to_string()))?;
    let encoded: String = token.iter().map(|byte| format!("{byte:02x}")).collect();
    let instance = format!("join-{encoded}");
    let properties = [("room", room_id), ("token", encoded.as_str())];
    let mut service = ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &format!("{instance}.local."),
        addresses.as_slice(),
        port,
        &properties[..],
    )
    .map_err(io::Error::other)?;
    service.set_requires_probe(false);
    service.set_interfaces(addresses.into_iter().map(IfKind::Addr).collect());
    let offer = JoinOffer {
        daemon: ServiceDaemon::new().map_err(io::Error::other)?,
        name: service.get_fullname().to_owned(),
    };
    offer.daemon.register(service).map_err(io::Error::other)?;

    let deadline = Instant::now() + OFFER_TIMEOUT;
    while Instant::now() < deadline && !cancelled() {
        for listener in &listeners {
            if let Ok((mut stream, SocketAddr::V4(peer))) = listener.accept()
                && expected_hosts.iter().any(|host| host.ip() == peer.ip())
            {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(TOKEN_TIMEOUT))?;
                stream.set_write_timeout(Some(TOKEN_TIMEOUT))?;
                let mut proof = [0; 20];
                proof[..4].copy_from_slice(&RETURN_MAGIC);
                proof[4..].copy_from_slice(&token);
                let mut response = [0; 16];
                if stream.write_all(&proof).is_ok()
                    && stream.read_exact(&mut response).is_ok()
                    && response == token
                {
                    return Ok(stream);
                }
            }
        }
        thread::sleep(Duration::from_millis(30));
    }
    Err(io::Error::new(
        if cancelled() {
            io::ErrorKind::Interrupted
        } else {
            io::ErrorKind::TimedOut
        },
        "The broadcaster could not establish a return connection",
    ))
}

struct JoinOffer {
    daemon: ServiceDaemon,
    name: String,
}

impl Drop for JoinOffer {
    fn drop(&mut self) {
        if let Ok(status) = self.daemon.unregister(&self.name) {
            let _ = status.recv_timeout(Duration::from_millis(100));
        }
        let _ = self.daemon.shutdown();
    }
}

fn parse_token(encoded: &str) -> Option<[u8; 16]> {
    if encoded.len() != 32 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut token = [0; 16];
    for (index, byte) in token.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&encoded[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(token)
}
