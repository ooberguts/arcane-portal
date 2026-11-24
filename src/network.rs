use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket,
};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use if_addrs::{IfAddr, Ifv4Addr, Interface};
use mdns_sd::{
    IfKind, IfPredicate, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo,
};
use serde::{Deserialize, Serialize};
use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
use zune_jpeg::JpegDecoder;

use crate::Frame;
use crate::audio::PACKET_BYTES;
use crate::video::{self, Feedback, Packet, Playout};

const DEFAULT_PORT: u16 = 48742;
const SERVICE_TYPE: &str = "_arcane-portal._tcp.local.";
const VERSION: &str = "1";
const MAGIC: [u8; 8] = *b"APLAN01\n";
const FAST_MAGIC: [u8; 8] = *b"APLAN02\n";
const MOTION_MAGIC: [u8; 8] = *b"APLAN03\n";
const AUDIO_MAGIC: [u8; 8] = *b"APAUD01\n";
const FRAME_REQUEST: [u8; 1] = [1];
const VIDEO_CREDITS: usize = 2;
const MOTION_CREDITS: usize = 3;
const MAX_MOTION_FRAMES: usize = 18;
const MOTION_DEADLINE: Duration = Duration::from_millis(250);
const AUDIO_BUFFER: usize = 6;
const AUDIO_FRESHNESS: Duration = Duration::from_millis(200);
const FPS_WINDOW: Duration = Duration::from_secs(1);
const MAX_TEXT_BYTES: usize = 120;
const MAX_METADATA_BYTES: usize = 2_048;
const MAX_VIDEO_BYTES: usize = 8 * 1024 * 1024;
const MAX_VIEWERS: usize = 8;
const MAX_ROOMS: usize = 128;
const MAX_ROOM_ADDRESSES: usize = 8;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const FRAME_TIMEOUT: Duration = Duration::from_secs(7);
const IO_TIMEOUT: Duration = Duration::from_secs(8);
const LISTEN_INTERVAL: Duration = Duration::from_millis(30);
const ADVERTISE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Room {
    pub address: SocketAddrV4,
    alternatives: Vec<SocketAddrV4>,
    service_id: Option<String>,
    fast_video: bool,
    motion_video: bool,
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
            alternatives: Vec::new(),
            service_id: None,
            fast_video: false,
            motion_video: false,
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
                            let room = discovered_room(&service, &active_lan_interfaces());
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

fn discovered_room(service: &ResolvedService, interfaces: &[Interface]) -> Option<Room> {
    if service.get_property_val_str("v")? != VERSION || service.get_port() == 0 {
        return None;
    }
    let mut addresses: Vec<_> = service
        .get_addresses()
        .iter()
        .filter_map(|scoped| {
            let ScopedIp::V4(scoped) = scoped else {
                return None;
            };
            let ip = *scoped.addr();
            let route = interfaces
                .iter()
                .filter_map(|interface| {
                    let address = lan_address(interface)?;
                    if !same_lan(address, ip) {
                        return None;
                    }
                    let received_here = scoped.interface_ids().iter().any(|source| {
                        interface.index == Some(source.index) || interface.name == source.name
                    });
                    Some((received_here, address.prefixlen))
                })
                .max()?;
            Some((ip.is_link_local(), Reverse(route), ip))
        })
        .collect();
    addresses.sort_unstable();
    let mut endpoints = Vec::new();
    for (_, _, ip) in addresses {
        let endpoint = SocketAddrV4::new(ip, service.get_port());
        if !endpoints.contains(&endpoint) {
            endpoints.push(endpoint);
        }
        if endpoints.len() == MAX_ROOM_ADDRESSES {
            break;
        }
    }
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
    let mut endpoints = endpoints.into_iter();
    let mut room = details.into_room(endpoints.next()?, viewers.min(MAX_VIEWERS));
    room.alternatives = endpoints.collect();
    room.service_id = Some(service.get_fullname().to_owned());
    room.fast_video = service.get_property_val_str("video") == Some("2");
    room.motion_video = service.get_property_val_str("codec") == Some("h264");
    Some(room)
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

fn active_lan_interfaces() -> Vec<Interface> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| lan_address(interface).is_some())
        .collect()
}

fn lan_interfaces() -> Vec<Ifv4Addr> {
    active_lan_interfaces()
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

pub(crate) fn same_lan(address: &Ifv4Addr, peer: Ipv4Addr) -> bool {
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

        let state = Arc::new(HostState {
            data: Mutex::new(HostData {
                details,
                stopped: false,
                latest: None,
                motion: VecDeque::new(),
                motion_sequence: 0,
                keyframe_requested: false,
                last_congestion: None,
                clients: HashMap::new(),
                next_client: 0,
                viewers: 0,
                fps: FrameRate::default(),
                audio: VecDeque::new(),
                audio_sequence: 0,
                audio_generation: 0,
            }),
            ready: Condvar::new(),
            audio_ready: Condvar::new(),
        });
        let host = Arc::clone(&state);
        let lan_addresses = addresses.clone();
        let listener = thread::Builder::new()
            .name("arcane-host".into())
            .spawn(move || run_host(&host, &listeners, lan_addresses))?;
        if addresses.is_empty() {
            addresses.push(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        }

        Ok(Self {
            state,
            addresses,
            listener: Some(listener),
        })
    }

    #[cfg(test)]
    fn publisher(&self) -> impl Fn(Arc<[u8]>) + Send + 'static + use<> {
        let state = Arc::clone(&self.state);
        move |jpeg| {
            if jpeg.is_empty() || jpeg.len() > MAX_VIDEO_BYTES {
                return;
            }
            let mut data = state.data.lock().unwrap();
            if !data.stopped {
                let sequence = data.latest.as_ref().map_or(1, |(sequence, _)| sequence + 1);
                data.latest = Some((sequence, jpeg));
                data.fps.record();
                state.ready.notify_all();
            }
        }
    }

    pub fn video_publisher(&self) -> VideoPublisher {
        VideoPublisher(Arc::clone(&self.state))
    }

    pub fn viewers(&self) -> usize {
        self.state.data.lock().unwrap().viewers
    }

    pub fn stream_fps(&self) -> f32 {
        self.state.data.lock().unwrap().fps.get()
    }

    pub fn audio_publisher(&self) -> impl Fn(Arc<[u8]>) + Send + 'static + use<> {
        let state = Arc::clone(&self.state);
        let generation = {
            let mut data = state.data.lock().unwrap();
            data.audio_generation = data.audio_generation.wrapping_add(1);
            data.audio.clear();
            data.audio_generation
        };
        move |samples| {
            if samples.len() != PACKET_BYTES {
                return;
            }
            let mut data = state.data.lock().unwrap();
            if !data.stopped && data.audio_generation == generation {
                data.audio_sequence += 1;
                let sequence = data.audio_sequence;
                if data.audio.len() == AUDIO_BUFFER {
                    data.audio.pop_front();
                }
                data.audio.push_back(AudioPacket {
                    sequence,
                    samples,
                    captured: Instant::now(),
                });
                state.audio_ready.notify_all();
            }
        }
    }

    pub fn clear_audio(&self) {
        let mut data = self.state.data.lock().unwrap();
        data.audio_generation = data.audio_generation.wrapping_add(1);
        data.audio.clear();
    }

    pub fn set_format(&self, height: u32, fps: u32) {
        if (1..=1080).contains(&height) && (1..=60).contains(&fps) {
            let mut data = self.state.data.lock().unwrap();
            data.details.height = height;
            data.details.fps = fps;
        }
    }

    pub fn network_fps_limit(&self) -> Option<u32> {
        self.state
            .data
            .lock()
            .unwrap()
            .clients
            .values()
            .filter_map(|connection| {
                if connection.channel != Some(Channel::LegacyVideo) {
                    return None;
                }
                let delivery = connection.delivery.as_ref()?;
                (delivery.samples >= 3 && delivery.updated.elapsed() <= IO_TIMEOUT)
                    .then_some(delivery.average / u32::from(delivery.credits))
            })
            .max()
            .map(|delay| (1.0 / delay.as_secs_f64()).floor().clamp(1.0, 240.0) as u32)
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
            for connection in data.clients.values() {
                let _ = connection.socket.shutdown(Shutdown::Both);
            }
            self.state.ready.notify_all();
            self.state.audio_ready.notify_all();
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

struct HostState {
    data: Mutex<HostData>,
    ready: Condvar,
    audio_ready: Condvar,
}

pub(crate) struct VideoPublisher(Arc<HostState>);

impl VideoPublisher {
    pub fn feedback(&self) -> Feedback {
        let data = self.0.data.lock().unwrap();
        Feedback {
            keyframe: data.keyframe_requested,
            congested: data
                .last_congestion
                .is_some_and(|at| at.elapsed() < Duration::from_millis(1100)),
            legacy_viewers: data
                .clients
                .values()
                .any(|client| client.channel == Some(Channel::LegacyVideo)),
        }
    }

    pub fn publish(&self, motion: Option<Packet>, legacy: Option<Arc<[u8]>>) {
        let mut data = self.0.data.lock().unwrap();
        if data.stopped {
            return;
        }
        if let Some(packet) = motion
            .filter(|packet| !packet.bytes.is_empty() && packet.bytes.len() <= MAX_VIDEO_BYTES)
        {
            data.motion_sequence += 1;
            let sequence = data.motion_sequence;
            if packet.keyframe {
                data.keyframe_requested = false;
            }
            while data.motion.len() >= MAX_MOTION_FRAMES
                || data
                    .motion
                    .front()
                    .is_some_and(|frame| frame.published.elapsed() > MOTION_DEADLINE)
            {
                data.motion.pop_front();
            }
            data.motion.push_back(MotionFrame {
                sequence,
                packet,
                published: Instant::now(),
            });
            data.fps.record();
        }
        if let Some(jpeg) = legacy.filter(|jpeg| !jpeg.is_empty() && jpeg.len() <= MAX_VIDEO_BYTES)
        {
            let sequence = data.latest.as_ref().map_or(1, |(sequence, _)| sequence + 1);
            data.latest = Some((sequence, jpeg));
        }
        self.0.ready.notify_all();
    }
}

struct MotionFrame {
    sequence: u64,
    packet: Packet,
    published: Instant,
}

struct HostData {
    details: RoomDetails,
    stopped: bool,
    latest: Option<(u64, Arc<[u8]>)>,
    motion: VecDeque<MotionFrame>,
    motion_sequence: u64,
    keyframe_requested: bool,
    last_congestion: Option<Instant>,
    clients: HashMap<u64, Connection>,
    next_client: u64,
    viewers: usize,
    fps: FrameRate,
    audio: VecDeque<AudioPacket>,
    audio_sequence: u64,
    audio_generation: u64,
}

#[derive(Default)]
struct FrameRate(VecDeque<Instant>);

impl FrameRate {
    fn record(&mut self) {
        let now = Instant::now();
        while self
            .0
            .front()
            .is_some_and(|time| now.duration_since(*time) >= FPS_WINDOW)
        {
            self.0.pop_front();
        }
        if self.0.len() == 120 {
            self.0.pop_front();
        }
        self.0.push_back(now);
    }

    fn get(&self) -> f32 {
        self.0
            .iter()
            .rev()
            .take_while(|time| time.elapsed() < FPS_WINDOW)
            .count() as f32
    }
}

struct AudioPacket {
    sequence: u64,
    samples: Arc<[u8]>,
    captured: Instant,
}

struct Connection {
    socket: TcpStream,
    delivery: Option<Delivery>,
    channel: Option<Channel>,
}

struct Delivery {
    average: Duration,
    samples: u8,
    updated: Instant,
    credits: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Video,
    LegacyVideo,
    Audio,
}

fn run_host(
    state: &Arc<HostState>,
    listeners: &[(TcpListener, Option<Ifv4Addr>)],
    addresses: Vec<SocketAddrV4>,
) {
    let mut advertisement = None;
    let mut checked_at = None::<Instant>;
    let mut announced = None;
    loop {
        let (stopped, has_frame, announcement) = {
            let data = state.data.lock().unwrap();
            (
                data.stopped,
                data.latest.is_some() || !data.motion.is_empty(),
                (
                    data.viewers,
                    data.details.height,
                    data.details.fps,
                    !data.motion.is_empty(),
                ),
            )
        };
        if stopped {
            break;
        }
        if !addresses.is_empty()
            && has_frame
            && announced != Some(announcement)
            && checked_at.is_none_or(|time| time.elapsed() >= ADVERTISE_INTERVAL)
        {
            checked_at = Some(Instant::now());
            if advertisement.is_none()
                && let Ok(advertising) = Advertisement::new(&addresses)
            {
                let interfaces = listeners
                    .iter()
                    .filter_map(|(_, network)| network.clone())
                    .collect();
                let host = Arc::downgrade(state);
                let room_id = format!("{}.{SERVICE_TYPE}", advertising.instance);
                let _ = crate::reverse::watch(
                    &advertising.daemon,
                    room_id,
                    interfaces,
                    move |socket| {
                        if let Some(host) = host.upgrade() {
                            let _ = start_client(socket, &host);
                        }
                    },
                );
                advertisement = Some(advertising);
            }
            if let Some(advertisement) = &advertisement {
                let details = state.data.lock().unwrap().details.clone();
                let viewers = announcement.0;
                if advertisement
                    .publish(&details, viewers, announcement.3)
                    .is_ok()
                {
                    announced = Some((viewers, details.height, details.fps, announcement.3));
                }
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
                    let _ = start_client(socket, state);
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

    fn publish(
        &self,
        details: &RoomDetails,
        viewers: usize,
        motion: bool,
    ) -> Result<(), mdns_sd::Error> {
        let properties = [
            ("v", VERSION.to_owned()),
            ("name", details.name.clone()),
            ("title", details.title.clone()),
            ("height", details.height.to_string()),
            ("fps", details.fps.to_string()),
            ("viewers", viewers.to_string()),
            ("video", "2".to_owned()),
            ("codec", if motion { "h264" } else { "jpeg" }.to_owned()),
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

impl Client {
    fn reserve_channel(&self, channel: Channel) -> io::Result<()> {
        let mut data = self.host.data.lock().unwrap();
        if data
            .clients
            .values()
            .filter(|client| {
                client.channel.is_some_and(|existing| {
                    (existing == Channel::Audio) == (channel == Channel::Audio)
                })
            })
            .count()
            >= MAX_VIEWERS
        {
            return Err(io::Error::other(
                "The broadcast has reached its viewer limit",
            ));
        }
        if let Some(connection) = data.clients.get_mut(&self.id) {
            connection.channel = Some(channel);
        }
        Ok(())
    }

    fn record_delivery(&self, sent_at: Instant, credits: u8) {
        let updated = Instant::now();
        let sample = updated.duration_since(sent_at);
        let mut data = self.host.data.lock().unwrap();
        let Some(connection) = data.clients.get_mut(&self.id) else {
            return;
        };
        if let Some(delivery) = &mut connection.delivery {
            delivery.average = (delivery.average * 7 + sample) / 8;
            delivery.samples = delivery.samples.saturating_add(1);
            delivery.updated = updated;
        } else {
            connection.delivery = Some(Delivery {
                average: sample,
                samples: 1,
                updated,
                credits,
            });
        }
    }
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

fn start_client(socket: TcpStream, host: &Arc<HostState>) -> io::Result<()> {
    let (id, details) = {
        let mut data = host.data.lock().unwrap();
        if data.stopped || data.clients.len() >= MAX_VIEWERS * 2 {
            return Ok(());
        }
        let id = data.next_client;
        data.clients.insert(
            id,
            Connection {
                socket: socket.try_clone()?,
                delivery: None,
                channel: None,
            },
        );
        data.next_client += 1;
        (id, data.details.clone())
    };
    let client = Client {
        host: Arc::clone(host),
        id,
        watching: false,
    };
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
    let mut magic = [0; MAGIC.len()];
    socket.read_exact(&mut magic)?;
    let credits = match magic {
        MAGIC => 1,
        FAST_MAGIC => VIDEO_CREDITS,
        MOTION_MAGIC => MOTION_CREDITS,
        AUDIO_MAGIC => {
            client.reserve_channel(Channel::Audio)?;
            write_handshake(&mut socket, &magic, details)?;
            return stream_audio(socket, &client.host);
        }
        _ => {
            return Err(invalid_data(
                "This is not a compatible Arcane Portal stream",
            ));
        }
    };
    let motion = magic == MOTION_MAGIC;
    client.reserve_channel(if motion {
        Channel::Video
    } else {
        Channel::LegacyVideo
    })?;
    write_handshake(&mut socket, &magic, details)?;
    socket.set_read_timeout(Some(IO_TIMEOUT))?;
    socket.set_write_timeout(Some(IO_TIMEOUT))?;

    let mut request = [0];
    let mut previous = 0;
    let mut sent_at = VecDeque::with_capacity(credits);
    loop {
        // Request one frame only after decoding the last. Slow viewers skip stale frames.
        socket.read_exact(&mut request)?;
        if request != FRAME_REQUEST {
            return Err(invalid_data("Unexpected frame request"));
        }
        if !motion
            && sent_at.len() == credits
            && let Some(acknowledged) = sent_at.pop_front()
        {
            // Each replenished credit acknowledges the earliest outstanding JPEG.
            client.record_delivery(acknowledged, credits as u8);
        }
        let (sequence, bytes, viewers, motion_header) = if motion {
            let (sequence, packet, viewers, reset) = wait_for_motion(&client.host, previous)?;
            (
                sequence,
                packet.bytes,
                viewers,
                Some((packet.timestamp_ms, reset)),
            )
        } else {
            let (sequence, jpeg, viewers) = wait_for_frame(&client.host, previous)?;
            (sequence, jpeg, viewers, None)
        };
        let viewers = if client.watching {
            viewers
        } else {
            let mut data = client.host.data.lock().unwrap();
            data.viewers += 1;
            client.watching = true;
            data.viewers
        };
        let mut header = [0; 14];
        header[..4].copy_from_slice(&(bytes.len() as u32).to_be_bytes());
        header[4] = viewers as u8;
        let header_len = if let Some((timestamp, reset)) = motion_header {
            header[5..13].copy_from_slice(&timestamp.to_be_bytes());
            header[13] = u8::from(reset);
            header.len()
        } else {
            5
        };
        if !motion {
            sent_at.push_back(Instant::now());
        }
        socket.write_all(&header[..header_len])?;
        socket.write_all(&bytes)?;
        previous = sequence;
    }
}

fn wait_for_motion(host: &HostState, previous: u64) -> io::Result<(u64, Packet, usize, bool)> {
    let deadline = Instant::now() + FRAME_TIMEOUT;
    let mut data = host.data.lock().unwrap();
    let mut resuming = previous == 0;
    loop {
        if data.stopped {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Broadcast stopped",
            ));
        }
        if !resuming && let Some(next) = data.motion.iter().find(|frame| frame.sequence > previous)
        {
            if next.sequence == previous + 1 && next.published.elapsed() <= MOTION_DEADLINE {
                return Ok((next.sequence, next.packet.clone(), data.viewers, false));
            }
            resuming = true;
            data.last_congestion = Some(Instant::now());
        }
        if resuming {
            if let Some(keyframe) = data.motion.iter().rev().find(|frame| {
                frame.sequence > previous
                    && frame.packet.keyframe
                    && frame.published.elapsed() <= MOTION_DEADLINE
            }) {
                return Ok((
                    keyframe.sequence,
                    keyframe.packet.clone(),
                    data.viewers,
                    true,
                ));
            }
            data.keyframe_requested = true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "No recent video frame arrived",
            ));
        }
        (data, _) = host.ready.wait_timeout(data, remaining).unwrap();
    }
}

fn write_handshake(
    socket: &mut TcpStream,
    magic: &[u8; 8],
    details: &RoomDetails,
) -> io::Result<()> {
    let metadata = serde_json::to_vec(details).map_err(io::Error::other)?;
    let mut reply = Vec::with_capacity(magic.len() + 2 + metadata.len());
    reply.extend_from_slice(magic);
    reply.extend_from_slice(&(metadata.len() as u16).to_be_bytes());
    reply.extend_from_slice(&metadata);
    socket.write_all(&reply)
}

fn stream_audio(mut socket: TcpStream, host: &HostState) -> io::Result<()> {
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut previous = 0;
    let mut message = [0; PACKET_BYTES + 1];
    message[0] = 1;
    loop {
        let samples = {
            let data = host.data.lock().unwrap();
            let available = |packet: &AudioPacket| {
                packet.sequence > previous && packet.captured.elapsed() <= AUDIO_FRESHNESS
            };
            let (data, _) = host
                .audio_ready
                .wait_timeout_while(data, Duration::from_millis(500), |data| {
                    !data.stopped && !data.audio.back().is_some_and(available)
                })
                .unwrap();
            if data.stopped {
                return Ok(());
            }
            data.audio
                .iter()
                .find(|packet| available(packet))
                .map(|packet| {
                    previous = packet.sequence;
                    Arc::clone(&packet.samples)
                })
        };
        if let Some(samples) = samples {
            message[1..].copy_from_slice(&samples);
            socket.write_all(&message)?;
        } else {
            socket.write_all(&[0])?;
        }
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
    audio_socket: Option<TcpStream>,
    frame: Option<Arc<Frame>>,
    playout: Playout,
    decoded_video: bool,
    room: Option<Room>,
    error: Option<String>,
    fps: FrameRate,
    audio: VecDeque<Arc<[u8]>>,
    audio_error: Option<String>,
}

#[derive(Clone)]
struct JoinPlan {
    candidates: Vec<SocketAddrV4>,
    interfaces: Vec<Ifv4Addr>,
    identity: Option<(String, String)>,
    service_id: Option<String>,
    fast_video: bool,
    motion_video: bool,
}

impl JoinPlan {
    fn reverse(&self, cancelled: impl Fn() -> bool) -> io::Result<TcpStream> {
        let room_id = self
            .service_id
            .as_deref()
            .ok_or(io::ErrorKind::Unsupported)?;
        crate::reverse::receive(room_id, &self.candidates, &self.interfaces, cancelled)
    }
}

impl Viewer {
    pub fn join(room: &Room) -> Result<Self, String> {
        let candidates = std::iter::once(room.address)
            .chain(room.alternatives.iter().copied())
            .collect();
        let identity = (room.name.clone(), room.title.clone());
        Self::start(
            candidates,
            Some(identity),
            room.service_id.clone(),
            room.fast_video,
            room.motion_video,
        )
    }

    #[cfg(test)]
    fn connect(address: SocketAddrV4) -> Result<Self, String> {
        Self::start(vec![address], None, None, false, false)
    }

    fn start(
        mut candidates: Vec<SocketAddrV4>,
        identity: Option<(String, String)>,
        service_id: Option<String>,
        fast_video: bool,
        motion_video: bool,
    ) -> Result<Self, String> {
        let interfaces = lan_interfaces();
        candidates.retain(|address| {
            address.port() != 0
                && (address.ip().is_loopback()
                    || interfaces
                        .iter()
                        .any(|interface| same_lan(interface, *address.ip())))
        });
        if candidates.is_empty() {
            return Err("This room is no longer reachable on your local network.".into());
        }
        let state = Arc::new(Mutex::new(ViewerState::default()));
        let worker = Arc::clone(&state);
        let plan = JoinPlan {
            candidates,
            interfaces,
            identity,
            service_id,
            fast_video,
            motion_video,
        };
        thread::Builder::new()
            .name("arcane-viewer".into())
            .spawn(move || run_viewer(&worker, &plan))
            .map_err(|error| format!("Could not start the viewer. {error}"))?;
        Ok(Self { state })
    }

    pub fn frame(&self) -> Option<Arc<Frame>> {
        let mut state = self.state.lock().unwrap();
        if let Some(frame) = state.playout.due(Instant::now()) {
            state.frame = Some(frame);
        }
        state.frame.clone()
    }

    pub fn room(&self) -> Option<Room> {
        self.state.lock().unwrap().room.clone()
    }

    pub fn error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }

    pub fn stream_fps(&self) -> f32 {
        self.state.lock().unwrap().fps.get()
    }

    pub fn take_audio_packets(&self) -> Vec<Arc<[u8]>> {
        self.state.lock().unwrap().audio.drain(..).collect()
    }

    pub fn audio_error(&self) -> Option<String> {
        self.state.lock().unwrap().audio_error.clone()
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.stopped = true;
        let sockets = [state.socket.take(), state.audio_socket.take()];
        for socket in sockets.into_iter().flatten() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

#[derive(Default)]
struct JoinDiagnostics {
    direct: Option<io::Error>,
    first_frame: Option<io::Error>,
}

fn run_viewer(viewer: &Arc<Mutex<ViewerState>>, plan: &JoinPlan) {
    let mut diagnostics = JoinDiagnostics::default();
    for &address in &plan.candidates {
        let result = TcpStream::connect_timeout(&SocketAddr::V4(address), HANDSHAKE_TIMEOUT)
            .and_then(|socket| receive_frames(socket, address, plan, viewer, false));
        if finish_attempt(viewer, result, &mut diagnostics) {
            return;
        }
    }
    if plan.service_id.is_some() {
        let result = plan
            .reverse(|| viewer.lock().unwrap().stopped)
            .and_then(|socket| receive_frames(socket, plan.candidates[0], plan, viewer, true));
        if finish_attempt(viewer, result, &mut diagnostics) {
            return;
        }
    }

    let mut viewer = viewer.lock().unwrap();
    if viewer.stopped {
        return;
    }
    viewer.error = Some(if let Some(error) = diagnostics.first_frame {
        format!("The host connected, but did not send a usable first frame. {error}")
    } else {
        let detail = diagnostics
            .direct
            .map_or_else(String::new, |error| format!(" {error}"));
        format!(
            "The room was discovered, but the video connection failed. Check that both devices can communicate on the local network and that Arcane Portal is allowed through the broadcaster's firewall.{detail}"
        )
    });
}

fn finish_attempt(
    viewer: &Mutex<ViewerState>,
    result: io::Result<()>,
    diagnostics: &mut JoinDiagnostics,
) -> bool {
    let mut state = viewer.lock().unwrap();
    state.socket = None;
    if state.stopped {
        return true;
    }
    let Err(error) = result else {
        return true;
    };
    if state.decoded_video {
        state.error = Some(format!(
            "The broadcast ended or the connection was lost. {error}"
        ));
        state.audio.clear();
        if let Some(socket) = state.audio_socket.take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        return true;
    }
    if state.room.take().is_some() {
        diagnostics.first_frame.get_or_insert(error);
    } else {
        diagnostics.direct.get_or_insert(error);
    }
    false
}

fn receive_frames(
    mut socket: TcpStream,
    address: SocketAddrV4,
    plan: &JoinPlan,
    viewer: &Arc<Mutex<ViewerState>>,
    used_reverse: bool,
) -> io::Result<()> {
    {
        let mut state = viewer.lock().unwrap();
        if state.stopped {
            return Ok(());
        }
        state.socket = Some(socket.try_clone()?);
    }
    let (magic, credits) = if plan.motion_video {
        (MOTION_MAGIC, MOTION_CREDITS)
    } else if plan.fast_video {
        (FAST_MAGIC, VIDEO_CREDITS)
    } else {
        (MAGIC, 1)
    };
    let details = negotiate(&mut socket, &magic, plan.identity.as_ref())?;
    viewer.lock().unwrap().room = Some(details.into_room(address, 0));
    socket.set_read_timeout(Some(IO_TIMEOUT))?;
    socket.write_all(&[FRAME_REQUEST[0]; MOTION_CREDITS][..credits])?;

    let mut packet = Vec::new();
    let mut header = [0; 14];
    let header_len = if plan.motion_video { header.len() } else { 5 };
    let mut decoder = None::<openh264::decoder::Decoder>;
    let mut first_frame = true;
    loop {
        socket.read_exact(&mut header[..header_len])?;
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        if length == 0 || length > MAX_VIDEO_BYTES {
            return Err(invalid_data("Invalid screen frame size"));
        }
        packet.resize(length, 0);
        socket.read_exact(&mut packet)?;
        let reset = plan.motion_video && header[13] != 0;
        let frame = if plan.motion_video {
            if reset || decoder.is_none() {
                decoder = Some(openh264::decoder::Decoder::new().map_err(io::Error::other)?);
            }
            video::decode(decoder.as_mut().unwrap(), &packet).map_err(io::Error::other)?
        } else {
            Some(decode_frame(&packet)?)
        };
        let Some(frame) = frame else {
            socket.write_all(&FRAME_REQUEST)?;
            continue;
        };
        let height = frame.height;
        {
            let mut state = viewer.lock().unwrap();
            if state.stopped {
                return Ok(());
            }
            state.decoded_video = true;
            if plan.motion_video {
                if reset {
                    state.playout = Playout::default();
                }
                let timestamp = u64::from_be_bytes(header[5..13].try_into().unwrap());
                state
                    .playout
                    .push(timestamp, Instant::now(), Arc::new(frame));
            } else {
                state.frame = Some(Arc::new(frame));
            }
            state.fps.record();
            if let Some(room) = &mut state.room {
                room.height = height;
                room.viewers = usize::from(header[4]).min(MAX_VIEWERS);
            }
        }
        if first_frame && plan.fast_video {
            start_audio_receiver(viewer, plan, address, used_reverse);
            first_frame = false;
        }
        socket.write_all(&FRAME_REQUEST)?;
    }
}

fn negotiate(
    socket: &mut TcpStream,
    magic: &[u8; 8],
    identity: Option<&(String, String)>,
) -> io::Result<RoomDetails> {
    socket.set_nonblocking(false)?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    socket.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    socket.write_all(magic)?;
    let mut response = [0; 8];
    socket.read_exact(&mut response)?;
    if response != *magic {
        return Err(invalid_data(
            "This is not a compatible Arcane Portal stream",
        ));
    }
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
    if identity.is_some_and(|(name, title)| details.name != *name || details.title != *title) {
        return Err(invalid_data(
            "The host is now sharing a different broadcast",
        ));
    }
    Ok(details)
}

fn start_audio_receiver(
    viewer: &Arc<Mutex<ViewerState>>,
    plan: &JoinPlan,
    address: SocketAddrV4,
    used_reverse: bool,
) {
    let plan = plan.clone();
    let state = Arc::clone(viewer);
    if let Err(error) = thread::Builder::new()
        .name("arcane-viewer-audio".into())
        .spawn(move || {
            let cancelled = || {
                let state = state.lock().unwrap();
                state.stopped || state.error.is_some()
            };
            let direct = (!used_reverse).then(|| {
                let mut socket = TcpStream::connect_timeout(&address.into(), HANDSHAKE_TIMEOUT)?;
                negotiate(&mut socket, &AUDIO_MAGIC, plan.identity.as_ref())?;
                Ok::<TcpStream, io::Error>(socket)
            });
            let connection = direct.and_then(Result::ok).map_or_else(
                || {
                    let mut socket = plan.reverse(cancelled)?;
                    negotiate(&mut socket, &AUDIO_MAGIC, plan.identity.as_ref())?;
                    Ok(socket)
                },
                Ok,
            );
            let result = connection.and_then(|socket| receive_audio(socket, &state));
            let mut state = state.lock().unwrap();
            state.audio_socket = None;
            if let Err(error) = result
                && !state.stopped
                && state.error.is_none()
            {
                state.audio_error = Some(format!(
                    "Could not receive the broadcaster's audio. {error}"
                ));
            }
        })
    {
        viewer.lock().unwrap().audio_error =
            Some(format!("Could not start the audio connection. {error}"));
    }
}

fn receive_audio(mut socket: TcpStream, viewer: &Mutex<ViewerState>) -> io::Result<()> {
    socket.set_read_timeout(Some(IO_TIMEOUT))?;
    {
        let mut state = viewer.lock().unwrap();
        if state.stopped || state.error.is_some() {
            return Ok(());
        }
        state.audio_socket = Some(socket.try_clone()?);
    }
    let mut kind = [0; 1];
    let mut samples = [0; PACKET_BYTES];
    loop {
        socket.read_exact(&mut kind)?;
        match kind[0] {
            0 => continue,
            1 => socket.read_exact(&mut samples)?,
            _ => return Err(invalid_data("Invalid system audio packet")),
        }
        let mut state = viewer.lock().unwrap();
        if state.stopped || state.error.is_some() {
            return Ok(());
        }
        if state.audio.len() == AUDIO_BUFFER {
            state.audio.pop_front();
        }
        state.audio.push_back(Arc::from(samples));
    }
}

fn decode_frame(jpeg: &[u8]) -> io::Result<Frame> {
    let options = DecoderOptions::default()
        .set_max_width(4096)
        .set_max_height(1600)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(jpeg), options);
    decoder
        .decode_headers()
        .map_err(|_| invalid_data("Invalid JPEG frame"))?;
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| invalid_data("Missing screen dimensions"))?;
    if width == 0 || height == 0 || width > 4096 || height > 1600 || width * height > 4_000_000 {
        return Err(invalid_data("Invalid screen frame dimensions"));
    }
    let rgba = decoder
        .decode()
        .map_err(|_| invalid_data("Could not decode the JPEG frame"))?;
    if rgba.len() != width * height * 4 {
        return Err(invalid_data("Invalid decoded screen frame length"));
    }
    Ok(Frame {
        width: width as u32,
        height: height as u32,
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
    fn tries_another_advertised_address_when_first_frame_fails() {
        let details = RoomDetails::new("Host", "Shared window", 720, 24).unwrap();
        let first = Host::listen(details.clone(), 0, Vec::new()).unwrap();
        let second = Host::listen(details.clone(), 0, Vec::new()).unwrap();
        (first.publisher())(Arc::from(&b"invalid JPEG"[..]));
        (second.publisher())(test_frame().into());
        let mut room = details.into_room(first.addresses()[0], 0);
        room.alternatives.push(second.addresses()[0]);

        let viewer = Viewer::join(&room).unwrap();
        eventually(|| viewer.frame().is_some());
        assert_eq!(viewer.room().unwrap().address, second.addresses()[0]);
        assert!(viewer.error().is_none());
    }

    #[test]
    fn existing_viewers_can_decode_a_new_size_after_a_format_change() {
        let details = RoomDetails::new("Host", "Shared window", 720, 24).unwrap();
        let host = Host::listen(details, 0, Vec::new()).unwrap();
        let address = host.addresses()[0];
        let publish = host.publisher();
        publish(test_frame().into());
        let viewer = Viewer::connect(address).unwrap();
        eventually(|| viewer.frame().is_some());
        assert_eq!(viewer.room().unwrap().fps, 24);

        host.set_format(1080, 60);
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg)
            .encode(&[121; 4 * 4 * 3], 4, 4, ExtendedColorType::Rgb8)
            .unwrap();
        let jpeg: Arc<[u8]> = jpeg.into();
        eventually(|| {
            publish(jpeg.clone());
            host.network_fps_limit().is_some()
                && viewer.frame().is_some_and(|frame| frame.height == 4)
        });
        assert!(viewer.error().is_none());

        let new_viewer = Viewer::connect(address).unwrap();
        eventually(|| new_viewer.frame().is_some());
        assert_eq!(new_viewer.room().unwrap().fps, 60);
        assert_eq!(new_viewer.room().unwrap().height, 4);
    }

    #[test]
    fn pipelined_video_and_optional_audio_share_a_room() {
        let details = RoomDetails::new("Host", "Audio and video", 720, 60).unwrap();
        let host = Host::listen(details.clone(), 0, Vec::new()).unwrap();
        let mut room = details.into_room(host.addresses()[0], 0);
        room.fast_video = true;
        let video = host.publisher();
        let jpeg: Arc<[u8]> = test_frame().into();
        video(jpeg.clone());
        let viewer = Viewer::join(&room).unwrap();
        eventually(|| viewer.frame().is_some());
        let audio = host.audio_publisher();
        let samples: Arc<[u8]> = vec![19; PACKET_BYTES].into();
        eventually(|| {
            video(jpeg.clone());
            audio(samples.clone());
            viewer
                .take_audio_packets()
                .iter()
                .any(|packet| packet.as_ref() == samples.as_ref())
        });
        assert!(viewer.stream_fps() > 0.0 && host.stream_fps() > 0.0);
        assert_eq!(host.viewers(), 1);
        host.clear_audio();
        audio(vec![77; PACKET_BYTES].into());
        assert!(host.state.data.lock().unwrap().audio.is_empty());
        assert!(viewer.audio_error().is_none());
    }

    #[test]
    fn modern_viewer_keeps_order_through_live_codec_changes() {
        let details = RoomDetails::new("Host", "Motion", 720, 24).unwrap();
        let host = Host::listen(details.clone(), 0, Vec::new()).unwrap();
        let publish = host.video_publisher();
        let mut codec = video::Encoder::default();
        let now = Instant::now();
        codec.update_picture(128, 72, &vec![95; 128 * 72 * 4]);
        publish.publish(codec.encode(24, 0, now, &publish.feedback()).unwrap(), None);
        let mut room = details.into_room(host.addresses()[0], 0);
        room.motion_video = true;
        room.fast_video = true;
        let viewer = Viewer::join(&room).unwrap();
        eventually(|| viewer.frame().is_some_and(|frame| frame.height == 72));
        codec.update_picture(96, 64, &vec![184; 96 * 64 * 4]);
        publish.publish(
            codec
                .encode(30, 140, Instant::now(), &publish.feedback())
                .unwrap(),
            None,
        );
        eventually(|| viewer.frame().is_some_and(|frame| frame.height == 64));

        let legacy = Viewer::connect(room.address).unwrap();
        eventually(|| publish.feedback().legacy_viewers);
        publish.publish(None, Some(test_frame().into()));
        eventually(|| legacy.frame().is_some());
        assert_eq!(host.viewers(), 2);
        assert!(viewer.error().is_none());
    }

    #[test]
    fn lagging_motion_viewer_can_resume_only_from_a_keyframe() {
        let details = RoomDetails::new("Host", "Motion", 720, 24).unwrap();
        let host = Host::listen(details, 0, Vec::new()).unwrap();
        let publish = host.video_publisher();
        for index in 0..MAX_MOTION_FRAMES {
            publish.publish(
                Some(Packet {
                    bytes: Arc::from([1u8]),
                    timestamp_ms: index as u64,
                    keyframe: false,
                }),
                None,
            );
        }
        publish.publish(
            Some(Packet {
                bytes: Arc::from([2u8]),
                timestamp_ms: 100,
                keyframe: true,
            }),
            None,
        );
        publish.publish(
            Some(Packet {
                bytes: Arc::from([3u8]),
                timestamp_ms: 140,
                keyframe: false,
            }),
            None,
        );
        let (sequence, keyframe, _, reset) = wait_for_motion(&host.state, 1).unwrap();
        assert!(reset && keyframe.keyframe && publish.feedback().congested);
        let (_, next, _, reset) = wait_for_motion(&host.state, sequence).unwrap();
        assert!(!reset && next.bytes.as_ref() == [3]);
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
        let viewer = Viewer::join(&room().unwrap()).unwrap();
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
    #[ignore = "requires an active private interface with multicast enabled"]
    fn discovered_room_uses_return_connection_when_direct_tcp_is_unavailable() {
        let interfaces = lan_interfaces();
        assert!(!interfaces.is_empty(), "no active private IPv4 interfaces");
        let discovery = Discovery::start().unwrap();
        let details = RoomDetails::new("Arcane test", "Return connection", 720, 24).unwrap();
        let host = Host::listen(details, 0, interfaces).unwrap();
        let addresses = host.addresses();
        (host.publisher())(test_frame().into());
        let find_room = || {
            discovery
                .rooms()
                .into_iter()
                .find(|room| room.title == "Return connection" && addresses.contains(&room.address))
        };
        eventually_for(Duration::from_secs(12), "room never appeared", || {
            find_room().is_some()
        });

        let unused_port = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut room = find_room().unwrap();
        room.address.set_port(unused_port);
        for address in &mut room.alternatives {
            address.set_port(unused_port);
        }
        let viewer = Viewer::join(&room).unwrap();
        eventually_for(
            Duration::from_secs(15),
            "no return video connection",
            || viewer.frame().is_some() || viewer.error().is_some(),
        );
        assert!(viewer.frame().is_some(), "{:?}", viewer.error());
        assert_eq!(viewer.room().unwrap().title, "Return connection");
        assert_eq!(host.viewers(), 1);
        let audio = host.audio_publisher();
        let packet: Arc<[u8]> = vec![48; PACKET_BYTES].into();
        eventually_for(Duration::from_secs(9), "no return audio connection", || {
            audio(packet.clone());
            viewer
                .take_audio_packets()
                .iter()
                .any(|received| received.as_ref() == packet.as_ref())
        });
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
