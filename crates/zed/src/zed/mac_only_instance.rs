use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    thread,
    time::Duration,
};

use futures::channel::mpsc::UnboundedSender;
use serde::{Deserialize, Serialize};

use sysinfo::System;

use release_channel::ReleaseChannel;

const LOCALHOST: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(10);
const RECEIVE_TIMEOUT: Duration = Duration::from_millis(35);
const SEND_TIMEOUT: Duration = Duration::from_millis(20);
const USER_BLOCK: u16 = 100;

fn address() -> SocketAddr {
    // These port numbers are offset by the user ID to avoid conflicts between
    // different users on the same machine. In addition to that the ports for each
    // release channel are spaced out by 100 to avoid conflicts between different
    // users running different release channels on the same machine. This ends up
    // interleaving the ports between different users and different release channels.
    //
    // On macOS user IDs start at 501 and on Linux they start at 1000. The first user
    // on a Mac with ID 501 running a dev channel build will use port 44238, and the
    // second user with ID 502 will use port 44239, and so on. User 501 will use ports
    // 44338, 44438, and 44538 for the preview, stable, and nightly channels,
    // respectively. User 502 will use ports 44339, 44439, and 44539 for the preview,
    // stable, and nightly channels, respectively.
    let port = match *release_channel::RELEASE_CHANNEL {
        ReleaseChannel::Dev => 43737,
        ReleaseChannel::Preview => 43737 + USER_BLOCK,
        ReleaseChannel::Stable => 43737 + (2 * USER_BLOCK),
        ReleaseChannel::Nightly => 43737 + (3 * USER_BLOCK),
    };
    let mut user_port = port;
    let mut sys = System::new_all();
    sys.refresh_all();
    if let Ok(current_pid) = sysinfo::get_current_pid()
        && let Some(uid) = sys
            .process(current_pid)
            .and_then(|process| process.user_id())
    {
        let uid_u32 = get_uid_as_u32(uid);
        // Ensure that the user ID is not too large to avoid overflow when
        // calculating the port number. This seems unlikely but it doesn't
        // hurt to be safe.
        let max_port = 65535;
        let max_uid: u32 = max_port - port as u32;
        let wrapped_uid: u16 = (uid_u32 % max_uid) as u16;
        user_port += wrapped_uid;
    }

    SocketAddr::V4(SocketAddrV4::new(LOCALHOST, user_port))
}

#[cfg(unix)]
fn get_uid_as_u32(uid: &sysinfo::Uid) -> u32 {
    *uid.clone()
}

#[cfg(windows)]
fn get_uid_as_u32(uid: &sysinfo::Uid) -> u32 {
    // Extract the RID which is an integer
    uid.to_string()
        .rsplit('-')
        .next()
        .and_then(|rid| rid.parse::<u32>().ok())
        .unwrap_or(0)
}

fn instance_handshake() -> &'static str {
    match *release_channel::RELEASE_CHANNEL {
        ReleaseChannel::Dev => "Asylum Editor Dev Instance Running",
        ReleaseChannel::Nightly => "Asylum Editor Nightly Instance Running",
        ReleaseChannel::Preview => "Asylum Editor Preview Instance Running",
        ReleaseChannel::Stable => "Asylum Editor Stable Instance Running",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsOnlyInstance {
    Yes,
    No,
}

/// What another process can ask a running instance for, sent as one line of JSON after the
/// handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstanceRequest {
    /// Come to the front, which is how switching to a profile that is already open works.
    Activate,
    /// Open these paths, which belong to this instance's profile, and come to the front.
    Open { paths: Vec<String> },
}

trait InstanceStream: Read + Write {}
impl<T: Read + Write> InstanceStream for T {}

/// Claims the instance lock of this process's profile. While this process runs, every
/// request another process sends it arrives on `instance_requests`.
pub fn ensure_only_instance(instance_requests: UnboundedSender<InstanceRequest>) -> IsOnlyInstance {
    if paths::active_profile_id().is_some() {
        return ensure_only_profile_instance(instance_requests);
    }

    if check_got_handshake() {
        return IsOnlyInstance::No;
    }

    let listener = match TcpListener::bind(address()) {
        Ok(listener) => listener,

        Err(err) => {
            log::warn!("Error binding to single instance port: {err}");
            if check_got_handshake() {
                return IsOnlyInstance::No;
            }

            // Avoid failing to start when some other application by chance already has
            // a claim on the port. This is sub-par as any other instance that gets launched
            // will be unable to communicate with this instance and will duplicate
            log::warn!("Backup handshake request failed, continuing without handshake");
            return IsOnlyInstance::Yes;
        }
    };

    thread::Builder::new()
        .name("EnsureSingleton".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => return,
                };

                _ = stream.set_nodelay(true);
                _ = stream.set_read_timeout(Some(SEND_TIMEOUT));
                answer_instance_request(&mut stream, &instance_requests);
            }
        })
        .unwrap();

    IsOnlyInstance::Yes
}

fn answer_instance_request(
    stream: &mut impl InstanceStream,
    instance_requests: &UnboundedSender<InstanceRequest>,
) {
    if stream.write_all(instance_handshake().as_bytes()).is_err() {
        return;
    }
    // Most connections are a starting instance checking for this one, which hangs up
    // right after the handshake, so a missing request is the normal case.
    let mut line = String::new();
    if BufReader::new(&mut *stream).read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let request = match serde_json::from_str::<InstanceRequest>(line.trim()) {
        Ok(request) => request,
        Err(err) => {
            log::warn!("Ignoring an unreadable instance request: {err}");
            return;
        }
    };
    // macOS ignores a background app asking to come forward, so the requester, which
    // is the active app, does the activating and needs this process's id for it.
    if let Err(err) = stream.write_all(std::process::id().to_string().as_bytes()) {
        log::warn!("Failed to send the process id to the requesting instance: {err}");
    }
    instance_requests.unbounded_send(request).ok();
}

fn check_got_handshake() -> bool {
    match TcpStream::connect_timeout(&address(), CONNECT_TIMEOUT) {
        Ok(mut stream) => {
            stream.set_read_timeout(Some(RECEIVE_TIMEOUT)).unwrap();
            read_handshake(&mut stream)
        }

        Err(_) => false,
    }
}

fn read_handshake(stream: &mut impl InstanceStream) -> bool {
    let mut buf = vec![0u8; instance_handshake().len()];
    if let Err(err) = stream.read_exact(&mut buf) {
        log::warn!("Connected to instance but failed to read the handshake: {err}");
        return false;
    }
    if buf == instance_handshake().as_bytes() {
        log::info!("Got instance handshake");
        return true;
    }
    log::warn!("Got wrong instance handshake value");
    false
}

fn profile_socket_path(profile_id: &str) -> PathBuf {
    paths::profiles_dir().join(profile_id).join("instance.sock")
}

// Profiles run side by side, so a non-default profile can't claim the port the
// default one uses. Its own data directory is unique to it, which makes a socket
// there a lock that only the processes of the same profile compete for.
fn ensure_only_profile_instance(
    instance_requests: UnboundedSender<InstanceRequest>,
) -> IsOnlyInstance {
    let socket_path = paths::data_dir().join("instance.sock");

    match UnixStream::connect(&socket_path) {
        Ok(mut stream) => {
            if let Err(err) = stream.set_read_timeout(Some(RECEIVE_TIMEOUT)) {
                log::warn!("Failed to set profile instance socket timeout: {err}");
            }
            if read_handshake(&mut stream) {
                return IsOnlyInstance::No;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            // Nobody is listening, so the socket was left behind by a process that died.
            if let Err(err) = std::fs::remove_file(&socket_path) {
                log::warn!("Failed to remove stale profile instance socket: {err}");
            }
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(err) => {
            log::warn!("Error binding profile instance socket, continuing without it: {err}");
            return IsOnlyInstance::Yes;
        }
    };

    let spawn_result = thread::Builder::new()
        .name("EnsureProfileSingleton".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    return;
                };
                if let Err(err) = stream.set_read_timeout(Some(SEND_TIMEOUT)) {
                    log::warn!("Failed to set profile instance socket timeout: {err}");
                }
                answer_instance_request(&mut stream, &instance_requests);
            }
        });
    if let Err(err) = spawn_result {
        log::warn!("Failed to spawn profile instance listener: {err}");
    }

    IsOnlyInstance::Yes
}

/// Connects to the running instance of `profile_id`, returning `None` when it isn't running.
/// Blocks for a few milliseconds, so it belongs on a background thread.
fn connect_to_profile(profile_id: &str) -> Option<Box<dyn InstanceStream>> {
    let mut stream: Box<dyn InstanceStream> = if profile_id == paths::DEFAULT_PROFILE_ID {
        let stream = TcpStream::connect_timeout(&address(), CONNECT_TIMEOUT).ok()?;
        stream.set_read_timeout(Some(RECEIVE_TIMEOUT)).ok()?;
        Box::new(stream)
    } else {
        let stream = UnixStream::connect(profile_socket_path(profile_id)).ok()?;
        stream.set_read_timeout(Some(RECEIVE_TIMEOUT)).ok()?;
        Box::new(stream)
    };
    read_handshake(&mut stream).then_some(stream)
}

/// Whether a process of `profile_id` is running.
pub fn is_profile_running(profile_id: &str) -> bool {
    connect_to_profile(profile_id).is_some()
}

/// Brings the running instance of `profile_id` to the front. Returns `false` when it isn't
/// running.
pub fn activate_profile(profile_id: &str) -> bool {
    send_to_profile(profile_id, &InstanceRequest::Activate)
}

/// Sends `request` to the running instance of `profile_id` and brings it to the front.
/// Returns `false` when it isn't running.
pub fn send_to_profile(profile_id: &str, request: &InstanceRequest) -> bool {
    let Some(mut stream) = connect_to_profile(profile_id) else {
        return false;
    };
    let mut line = match serde_json::to_string(request) {
        Ok(line) => line,
        Err(err) => {
            log::error!("Failed to encode the request for profile {profile_id}: {err}");
            return false;
        }
    };
    line.push('\n');
    if let Err(err) = stream.write_all(line.as_bytes()) {
        log::warn!("Failed to send the request to profile {profile_id}: {err}");
        return false;
    }
    let mut pid = String::new();
    if let Err(err) = stream.read_to_string(&mut pid) {
        log::warn!("Failed to read the process id of profile {profile_id}: {err}");
    }
    match pid.trim().parse::<i32>() {
        Ok(pid) => activate_process(pid, profile_id),
        Err(_) => log::warn!("Profile {profile_id} answered without a process id: {pid:?}"),
    }
    true
}

fn activate_process(pid: i32, profile_id: &str) {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};

    let activated = NSRunningApplication::runningApplicationWithProcessIdentifier(pid).is_some_and(
        |application| {
            application.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows)
        },
    );
    if !activated {
        log::warn!("Failed to bring profile {profile_id} (pid {pid}) to the front");
    }
}
