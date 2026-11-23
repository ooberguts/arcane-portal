use std::collections::HashMap;
use std::io::{self, Cursor, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket,
};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use if_addrs::{IfAddr, Ifv4Addr, Interface};
use image::{DynamicImage, ImageDecoder, codecs::jpeg::JpegDecoder};
use mdns_sd::{IfKind, IfPredicate, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};

use crate::Frame;

pub const DEFAULT_PORT: u16 = 48742;
const SERVICE_TYPE: &str = "_arcane-portal._tcp.local.";
const VERSION: &str = "1";
const MAGIC: [u8; 8] = *b"APLAN01\n";
const FRAME_REQUEST: [u8; 1] = [1];
const MAX_TEXT_BYTES: usize = 120;
const MAX_METADATA_BYTES: usize = 2_048;
const MAX_JPEG_BYTES: usize = 8 * 1024 * 1024;
const MAX_VIEWERS: usize = 8;
const MAX_ROOMS: usize = 128;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const FRAME_TIMEOUT: Duration = Duration::from_secs(7);
const IO_TIMEOUT: Duration = Duration::from_secs(8);
const LISTEN_INTERVAL: Duration = Duration::from_millis(30);
const ADVERTISE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Room {
    pub address: SocketAddrV4,
    pub name: String,
    pub title: String,
    pub height: u32,
    pub fps: u32,
    pub viewers: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RoomDetails {
    name: String,
    title: String,
    height: u32,
    fps: u32,
}

impl RoomDetails {
    fn new(name: &str, title: &str, height: u32, fps: u32) -> Option<Self> {
        let name = name.trim();
        let title = title.trim();
        let text_valid = |value: &str| {
            !value.is_empty()
                && value.len() <= MAX_TEXT_BYTES
                && !value.chars().any(char::is_control)
        };
        if !text_valid(name)
            || !text_valid(title)
            || !(1..=1080).contains(&height)
            || !(1..=60).contains(&fps)
        {
            return None;
        }
        Some(Self {
            name: name.to_owned(),
            title: title.to_owned(),
            height,
            fps,
        })
    }

    fn into_room(self, address: SocketAddrV4, viewers: usize) -> Room {
        Room {
            address,
            name: self.name,
            title: self.title,
            height: self.height,
            fps: self.fps,
            viewers,
        }
    }
}

pub struct Discovery {
    daemon: ServiceDaemon,
    rooms: Arc<Mutex<HashMap<String, Room>>>,
}

impl Discovery {
    pub fn start() -> Result<Self, String> {
        let daemon = start_mdns().map_err(|error| format!("Discovery unavailable. {error}"))?;
        let events = match daemon.browse(SERVICE_TYPE) {
            Ok(events) => events,
            Err(error) => {
                let _ = daemon.shutdown();
                return Err(format!("Could not browse the local network. {error}"));
            }
        };
        let rooms = Arc::new(Mutex::new(HashMap::new()));
        let room_list = Arc::clone(&rooms);
        if let Err(error) = thread::Builder::new()
            .name("arcane-discovery".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    match event {
                        ServiceEvent::ServiceResolved(service) => {
                            let key = service.get_fullname().to_owned();
                            let room = discovered_room(&service, &lan_interfaces());
                            let mut rooms = room_list.lock().unwrap();
                            if let Some(room) = room {
                                if rooms.contains_key(&key) || rooms.len() < MAX_ROOMS {
                                    rooms.insert(key, room);
                                }
                            } else {
                                rooms.remove(&key);
                            }
                        }
                        ServiceEvent::ServiceRemoved(_, key) => {
                            room_list.lock().unwrap().remove(&key);
                        }
                        _ => {}
                    }
                }
                room_list.lock().unwrap().clear();
            })
        {
            let _ = daemon.shutdown();
            return Err(format!("Could not start discovery. {error}"));
        }
        Ok(Self { daemon, rooms })
    }

    pub fn rooms(&self) -> Vec<Room> {
        let mut rooms: Vec<_> = self.rooms.lock().unwrap().values().cloned().collect();
        rooms.sort_by(|a, b| (&a.name, &a.title, a.address).cmp(&(&b.name, &b.title, b.address)));
        rooms
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

fn discovered_room(service: &ResolvedService, interfaces: &[Ifv4Addr]) -> Option<Room> {
    if service.get_property_val_str("v")? != VERSION || service.get_port() == 0 {
        return None;
    }
    let address = service
        .get_addresses_v4()
        .into_iter()
        .filter(|ip| interfaces.iter().any(|interface| same_lan(interface, *ip)))
        .min_by_key(|ip| (ip.is_link_local(), *ip))?;
    let details = RoomDetails::new(
        service.get_property_val_str("name")?,
        service.get_property_val_str("title")?,
        service.get_property_val_str("height")?.parse().ok()?,
        service.get_property_val_str("fps")?.parse().ok()?,
    )?;
    let viewers = service
        .get_property_val_str("viewers")?
        .parse::<usize>()
        .ok()?;
    Some(details.into_room(
        SocketAddrV4::new(address, service.get_port()),
        viewers.min(MAX_VIEWERS),
    ))
}

fn start_mdns() -> Result<ServiceDaemon, mdns_sd::Error> {
    let daemon = ServiceDaemon::new()?;
    let excluded_interfaces =
        IfKind::Predicate(IfPredicate::new(|iface| lan_address(iface).is_none()));
    if let Err(error) = daemon.disable_interface(excluded_interfaces) {
        let _ = daemon.shutdown();
        return Err(error);
    }
    Ok(daemon)
}

fn lan_interfaces() -> Vec<Ifv4Addr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .iter()
        .filter_map(lan_address)
        .cloned()
        .collect()
}

fn lan_address(interface: &Interface) -> Option<&Ifv4Addr> {
    match &interface.addr {
        IfAddr::V4(address)
            if interface.is_oper_up()
                && !interface.is_p2p()
                && address.prefixlen > 0
                && private_address(address.ip) =>
        {
            Some(address)
        }
        _ => None,
    }
}

fn private_address(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local()
}

fn same_lan(address: &Ifv4Addr, peer: Ipv4Addr) -> bool {
    let mask = u32::from(address.netmask);
    private_address(peer)
        && address.broadcast != Some(peer)
        && mask != 0
        && u32::from(address.ip) & mask == u32::from(peer) & mask
}

pub struct Host {
    state: Arc<HostState>,
    addresses: Vec<SocketAddrV4>,
    listener: Option<JoinHandle<()>>,
}

impl Host {
    pub fn start(name: &str, title: &str, height: u32, fps: u32) -> Result<Self, String> {
        let details = RoomDetails::new(name, title, height, fps).ok_or_else(|| {
            "Enter a display name and broadcast title of up to 120 UTF-8 bytes each.".to_string()
        })?;
        Self::listen(details, DEFAULT_PORT, lan_interfaces())
            .map_err(|error| format!("Could not open the broadcast. {error}"))
    }

    fn listen(details: RoomDetails, port: u16, interfaces: Vec<Ifv4Addr>) -> io::Result<Self> {
        let listener = bind_loopback(port)?;
        let port = listener.local_addr()?.port();
        let mut listeners = vec![(listener, None)];
        let mut addresses = Vec::new();
        for interface in interfaces {
            let address = SocketAddrV4::new(interface.ip, port);
            if addresses.contains(&address) {
                continue;
            }
            if let Ok(listener) = TcpListener::bind(address) {
                listener.set_nonblocking(true)?;
                listeners.push((listener, Some(interface)));
                addresses.push(address);
            }
        }
        let preferred = multicast_source();
        addresses.sort_by_key(|address| {
            (
                Some(*address.ip()) != preferred,
                address.ip().is_link_local(),
                *address,
            )
        });

        let state = Arc::new(HostState::default());
        let host = Arc::clone(&state);
        let lan_addresses = addresses.clone();
        let listener = thread::Builder::new()
            .name("arcane-host".into())
            .spawn(move || run_host(&host, &listeners, details, lan_addresses))?;
        if addresses.is_empty() {
            addresses.push(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        }

        Ok(Self {
            state,
            addresses,
            listener: Some(listener),
        })
    }

    pub fn publisher(&self) -> impl Fn(Arc<[u8]>) + Send + 'static + use<> {
        let state = Arc::clone(&self.state);
        move |jpeg| {
            if jpeg.is_empty() || jpeg.len() > MAX_JPEG_BYTES {
                return;
            }
            let mut data = state.data.lock().unwrap();
            if !data.stopped {
                let sequence = data.latest.as_ref().map_or(1, |(sequence, _)| sequence + 1);
                data.latest = Some((sequence, jpeg));
                state.ready.notify_all();
            }
        }
    }

    pub fn viewers(&self) -> usize {
        self.state.data.lock().unwrap().viewers
    }

    pub fn addresses(&self) -> Vec<SocketAddrV4> {
        self.addresses.clone()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        {
            let mut data = self.state.data.lock().unwrap();
            data.stopped = true;
            for socket in data.clients.values() {
                let _ = socket.shutdown(Shutdown::Both);
            }
            self.state.ready.notify_all();
        }
        if let Some(listener) = self.listener.take() {
            listener.thread().unpark();
            let _ = listener.join();
        }
    }
}

fn multicast_source() -> Option<Ipv4Addr> {
    // A UDP connect selects a local interface without sending any datagrams.
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(224, 0, 0, 251), 5353)).ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(address) => Some(*address.ip()),
        _ => None,
    }
}

fn bind_loopback(port: u16) -> io::Result<TcpListener> {
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
        }
        result => result?,
    };
    listener.set_nonblocking(true)?;
    Ok(listener)
}

#[derive(Default)]
struct HostState {
    data: Mutex<HostData>,
    ready: Condvar,
}

#[derive(Default)]
struct HostData {
    stopped: bool,
    latest: Option<(u64, Arc<[u8]>)>,
    clients: HashMap<u64, TcpStream>,
    next_client: u64,
    viewers: usize,
}

fn run_host(
    state: &Arc<HostState>,
    listeners: &[(TcpListener, Option<Ifv4Addr>)],
    details: RoomDetails,
    addresses: Vec<SocketAddrV4>,
) {
    let mut advertisement = None;
    let mut checked_at = None::<Instant>;
    let mut announced_viewers = None;
    loop {
        let (stopped, has_frame, viewers) = {
            let data = state.data.lock().unwrap();
            (data.stopped, data.latest.is_some(), data.viewers)
        };
        if stopped {
            break;
        }
        if !addresses.is_empty()
            && has_frame
            && announced_viewers != Some(viewers)
            && checked_at.is_none_or(|time| time.elapsed() >= ADVERTISE_INTERVAL)
        {
            checked_at = Some(Instant::now());
            if advertisement.is_none() {
                advertisement = Advertisement::new(&addresses).ok();
            }
            if let Some(advertisement) = &advertisement
                && advertisement.publish(&details, viewers).is_ok()
            {
                announced_viewers = Some(viewers);
            }
        }
        for (listener, network) in listeners {
            // Bound each pass so a busy socket cannot delay shutdown or starve another interface.
            for _ in 0..MAX_VIEWERS {
                let Ok((socket, SocketAddr::V4(peer))) = listener.accept() else {
                    break;
                };
                let permitted = match network {
                    Some(network) => same_lan(network, *peer.ip()),
                    None => peer.ip().is_loopback(),
                };
                if permitted {
                    let _ = start_client(socket, state, &details);
                }
            }
        }
        thread::park_timeout(LISTEN_INTERVAL);
    }
}

struct Advertisement {
    daemon: ServiceDaemon,
    instance: String,
    host_name: String,
    port: u16,
    addresses: Vec<IpAddr>,
}

impl Advertisement {
    fn new(addresses: &[SocketAddrV4]) -> Result<Self, mdns_sd::Error> {
        let daemon = start_mdns()?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let instance = format!("arcane-{:x}-{:x}", std::process::id(), timestamp.as_nanos());
        Ok(Self {
            daemon,
            host_name: format!("{instance}.local."),
            instance,
            port: addresses[0].port(),
            addresses: addresses
                .iter()
                .map(|address| IpAddr::V4(*address.ip()))
                .collect(),
        })
    }

    fn publish(&self, details: &RoomDetails, viewers: usize) -> Result<(), mdns_sd::Error> {
        let properties = [
            ("v", VERSION.to_owned()),
            ("name", details.name.clone()),
            ("title", details.title.clone()),
            ("height", details.height.to_string()),
            ("fps", details.fps.to_string()),
            ("viewers", viewers.to_string()),
        ];
        let mut service = ServiceInfo::new(
            SERVICE_TYPE,
            &self.instance,
            &self.host_name,
            self.addresses.as_slice(),
            self.port,
            properties.as_slice(),
        )?;
        // The process and timestamp identify the service, including across viewer-count updates.
        service.set_requires_probe(false);
        service.set_interfaces(self.addresses.iter().copied().map(IfKind::Addr).collect());
        self.daemon.register(service)
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        // Shutdown sends goodbye packets before acknowledging. Give the daemon time to
        // send them even when the desktop process is about to exit.
        if let Ok(acknowledgment) = self.daemon.shutdown() {
            let _ = acknowledgment.recv_timeout(Duration::from_millis(150));
        }
    }
}

struct Client {
    host: Arc<HostState>,
    id: u64,
    watching: bool,
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut data = self.host.data.lock().unwrap();
        data.clients.remove(&self.id);
        if self.watching {
            data.viewers -= 1;
        }
    }
}

fn start_client(socket: TcpStream, host: &Arc<HostState>, details: &RoomDetails) -> io::Result<()> {
    let id = {
        let mut data = host.data.lock().unwrap();
        if data.stopped || data.clients.len() >= MAX_VIEWERS {
            return Ok(());
        }
        let id = data.next_client;
        data.clients.insert(id, socket.try_clone()?);
        data.next_client += 1;
        id
    };
    let client = Client {
        host: Arc::clone(host),
        id,
        watching: false,
    };
    let details = details.clone();
    thread::Builder::new()
        .name("arcane-client".into())
        .spawn(move || {
            let _ = stream_frames(socket, client, &details);
        })?;
    Ok(())
}

fn stream_frames(
    mut socket: TcpStream,
    mut client: Client,
    details: &RoomDetails,
) -> io::Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    socket.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    read_magic(&mut socket)?;

    let metadata = serde_json::to_vec(details).map_err(io::Error::other)?;
    socket.write_all(&MAGIC)?;
    socket.write_all(&(metadata.len() as u16).to_be_bytes())?;
    socket.write_all(&metadata)?;
    socket.set_read_timeout(Some(IO_TIMEOUT))?;
    socket.set_write_timeout(Some(IO_TIMEOUT))?;

    let mut request = [0];
    let mut previous = 0;
    loop {
        // Request one frame only after decoding the last. Slow viewers skip stale frames.
        socket.read_exact(&mut request)?;
        if request != FRAME_REQUEST {
            return Err(invalid_data("Unexpected frame request"));
        }
        let (sequence, jpeg, viewers) = wait_for_frame(&client.host, previous)?;
        let viewers = if client.watching {
            viewers
        } else {
            let mut data = client.host.data.lock().unwrap();
            data.viewers += 1;
            client.watching = true;
            data.viewers
        };
        let mut header = [0; 5];
        header[..4].copy_from_slice(&(jpeg.len() as u32).to_be_bytes());
        header[4] = viewers as u8;
        socket.write_all(&header)?;
        socket.write_all(&jpeg)?;
        previous = sequence;
    }
}

fn wait_for_frame(host: &HostState, previous: u64) -> io::Result<(u64, Arc<[u8]>, usize)> {
    let data = host.data.lock().unwrap();
    let (data, _) = host
        .ready
        .wait_timeout_while(data, FRAME_TIMEOUT, |data| {
            !data.stopped
                && data
                    .latest
                    .as_ref()
                    .is_none_or(|(sequence, _)| *sequence == previous)
        })
        .unwrap();
    if data.stopped {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "Broadcast stopped",
        ));
    }
    match &data.latest {
        Some((sequence, jpeg)) if *sequence != previous => {
            Ok((*sequence, Arc::clone(jpeg), data.viewers))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "No screen frames arrived",
        )),
    }
}

pub struct Viewer {
    state: Arc<Mutex<ViewerState>>,
}

#[derive(Default)]
struct ViewerState {
    stopped: bool,
    socket: Option<TcpStream>,
    frame: Option<Arc<Frame>>,
    room: Option<Room>,
    error: Option<String>,
}

impl Viewer {
    pub fn connect(address: SocketAddrV4) -> Result<Self, String> {
        if address.port() == 0
            || (!address.ip().is_loopback()
                && !lan_interfaces()
                    .iter()
                    .any(|interface| same_lan(interface, *address.ip())))
        {
            return Err("Use a valid IPv4 address and port on your local network.".into());
        }
        let state = Arc::new(Mutex::new(ViewerState::default()));
        let worker = Arc::clone(&state);
        thread::Builder::new()
            .name("arcane-viewer".into())
            .spawn(move || {
                let result = receive_frames(address, &worker);
                let mut state = worker.lock().unwrap();
                state.socket = None;
                if !state.stopped
                    && let Err(error) = result
                {
                    let context = if state.room.is_some() {
                        "The broadcast ended or the connection was lost."
                    } else {
                        "Could not join this broadcast."
                    };
                    state.error = Some(format!("{context} {error}"));
                }
            })
            .map_err(|error| format!("Could not start the viewer. {error}"))?;
        Ok(Self { state })
    }

    pub fn frame(&self) -> Option<Arc<Frame>> {
        self.state.lock().unwrap().frame.clone()
    }

    pub fn room(&self) -> Option<Room> {
        self.state.lock().unwrap().room.clone()
    }

    pub fn error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.stopped = true;
        if let Some(socket) = state.socket.take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

fn receive_frames(address: SocketAddrV4, viewer: &Mutex<ViewerState>) -> io::Result<()> {
    let mut socket = TcpStream::connect_timeout(&SocketAddr::V4(address), HANDSHAKE_TIMEOUT)?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    socket.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    {
        let mut state = viewer.lock().unwrap();
        if state.stopped {
            return Ok(());
        }
        state.socket = Some(socket.try_clone()?);
    }
    socket.write_all(&MAGIC)?;
    read_magic(&mut socket)?;
    let mut length = [0; 2];
    socket.read_exact(&mut length)?;
    let length = usize::from(u16::from_be_bytes(length));
    if length == 0 || length > MAX_METADATA_BYTES {
        return Err(invalid_data("Invalid broadcast information size"));
    }
    let mut metadata = vec![0; length];
    socket.read_exact(&mut metadata)?;
    let details: RoomDetails = serde_json::from_slice(&metadata).map_err(io::Error::other)?;
    let details = RoomDetails::new(&details.name, &details.title, details.height, details.fps)
        .ok_or_else(|| invalid_data("Invalid broadcast information"))?;
    viewer.lock().unwrap().room = Some(details.into_room(address, 0));
    socket.set_read_timeout(Some(IO_TIMEOUT))?;

    let mut jpeg = Vec::new();
    let mut header = [0; 5];
    loop {
        socket.write_all(&FRAME_REQUEST)?;
        socket.read_exact(&mut header)?;
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        if length == 0 || length > MAX_JPEG_BYTES {
            return Err(invalid_data("Invalid screen frame size"));
        }
        jpeg.resize(length, 0);
        socket.read_exact(&mut jpeg)?;
        let frame = decode_frame(&jpeg)?;
        let mut state = viewer.lock().unwrap();
        if state.stopped {
            return Ok(());
        }
        state.frame = Some(Arc::new(frame));
        if let Some(room) = &mut state.room {
            room.viewers = usize::from(header[4]).min(MAX_VIEWERS);
        }
    }
}

fn read_magic(socket: &mut TcpStream) -> io::Result<()> {
    let mut magic = [0; MAGIC.len()];
    socket.read_exact(&mut magic)?;
    if magic == MAGIC {
        Ok(())
    } else {
        Err(invalid_data(
            "This is not a compatible Arcane Portal broadcast",
        ))
    }
}

fn decode_frame(jpeg: &[u8]) -> io::Result<Frame> {
    let decoder = JpegDecoder::new(Cursor::new(jpeg)).map_err(io::Error::other)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > 4096 || height > 1600 || width * height > 4_000_000 {
        return Err(invalid_data("Invalid screen frame dimensions"));
    }
    let rgba = DynamicImage::from_decoder(decoder)
        .map_err(io::Error::other)?
        .into_rgba8()
        .into_raw();
    Ok(Frame {
        width,
        height,
        rgba,
    })
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ExtendedColorType, codecs::jpeg::JpegEncoder};

    #[test]
    fn streams_jpeg_and_releases_viewers_on_loopback() {
        let details = RoomDetails::new("Host", "Shared window", 720, 24).unwrap();
        let host = Host::listen(details, 0, Vec::new()).unwrap();
        let address = host.addresses()[0];
        let publish = host.publisher();
        let jpeg = test_frame();
        publish(jpeg.clone().into());
        let viewer = Viewer::connect(address).unwrap();
        eventually(|| viewer.frame().is_some());
        let frame = viewer.frame().unwrap();
        assert_eq!((frame.width, frame.height, frame.rgba.len()), (2, 2, 16));
        assert_eq!(viewer.room().unwrap().name, "Host");
        assert_eq!(viewer.room().unwrap().viewers, 1);
        assert_eq!(host.viewers(), 1);

        drop(viewer);
        // Allow the host to finish its outstanding request and observe the closed connection.
        eventually(|| {
            publish(jpeg.clone().into());
            host.viewers() == 0
        });
        let viewer = Viewer::connect(address).unwrap();
        eventually(|| viewer.frame().is_some());
        publish(Arc::from(&b"invalid JPEG"[..]));
        eventually(|| viewer.error().is_some());
        assert!(Viewer::connect("8.8.8.8:48742".parse().unwrap()).is_err());
    }

    #[test]
    #[ignore = "requires an active private interface with multicast enabled"]
    fn discovers_live_rooms_and_removes_stopped_rooms() {
        let interfaces = lan_interfaces();
        assert!(!interfaces.is_empty(), "no active private IPv4 interfaces");
        let discovery = Discovery::start().unwrap();
        let details = RoomDetails::new("Arcane test", "Synthetic frames", 720, 24).unwrap();
        let host = Host::listen(details, 0, interfaces).unwrap();
        let addresses = host.addresses();
        let room = || {
            discovery.rooms().into_iter().find(|room| {
                room.name == "Arcane test"
                    && room.title == "Synthetic frames"
                    && addresses.contains(&room.address)
            })
        };
        assert!(room().is_none());
        let publish = host.publisher();
        let jpeg = test_frame();
        publish(jpeg.clone().into());
        eventually_for(Duration::from_secs(12), "room never appeared", || {
            room().is_some()
        });
        let viewer = Viewer::connect(room().unwrap().address).unwrap();
        eventually(|| viewer.frame().is_some());
        eventually_for(
            Duration::from_secs(6),
            "room viewer count never updated",
            || {
                publish(jpeg.clone().into());
                room().is_some_and(|room| room.viewers == 1)
            },
        );
        drop(host);
        eventually(|| viewer.error().is_some());
        eventually_for(
            Duration::from_secs(12),
            "room did not disappear after stopping",
            || room().is_none(),
        );
    }

    #[test]
    fn rejects_oversized_decoded_frames() {
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg)
            .encode(&vec![0; 4097 * 3], 4097, 1, ExtendedColorType::Rgb8)
            .unwrap();
        assert_eq!(
            decode_frame(&jpeg).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
    }

    fn eventually(check: impl FnMut() -> bool) {
        eventually_for(HANDSHAKE_TIMEOUT, "condition never became true", check);
    }

    fn eventually_for(timeout: Duration, failure: &str, mut check: impl FnMut() -> bool) {
        let start = Instant::now();
        while !check() {
            assert!(start.elapsed() < timeout, "{failure}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn test_frame() -> Vec<u8> {
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg)
            .encode(&[235; 2 * 2 * 3], 2, 2, ExtendedColorType::Rgb8)
            .unwrap();
        jpeg
    }
}
