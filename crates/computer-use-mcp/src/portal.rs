use std::{
    collections::HashMap,
    fs::{self, File},
    future::Future,
    io::{Read, Write},
    os::{
        fd::OwnedFd,
        unix::{fs::PermissionsExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use ashpd::desktop::{remote_desktop::RemoteDesktop, screencast::Screencast};
use futures_util::StreamExt;
use rustix::fs::{AtFlags, Mode, OFlags};
use tokio::sync::watch;
use zbus::{
    Connection, MatchRule, MessageStream, Proxy,
    message::Type as MessageType,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

use crate::capture::CaptureTarget;

const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
const REQUIRED_DEVICES: u32 = 3;
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";
pub(crate) const REMOTE_DESKTOP_INTERFACE: &str = "org.freedesktop.portal.RemoteDesktop";
pub(crate) const PORTAL_OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const TOKEN_MAX_BYTES: usize = 16 * 1024;
static SESSION_GENERATION: AtomicU64 = AtomicU64::new(0);

fn next_session_generation(counter: &AtomicU64) -> Result<u64, String> {
    let previous = counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
            generation.checked_add(1)
        })
        .map_err(|_| "portal session generation overflow".to_owned())?;
    previous
        .checked_add(1)
        .ok_or_else(|| "portal session generation overflow".to_owned())
}

#[derive(Debug, Clone)]
pub struct PortalConfig {
    pub persist_restore_token: bool,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self {
            persist_restore_token: true,
        }
    }
}

impl PortalConfig {
    pub fn from_env() -> Self {
        Self::from_persistence_value(
            std::env::var("COMPUTER_USE_MCP_PERSIST_PORTAL")
                .ok()
                .as_deref(),
        )
    }

    fn from_persistence_value(value: Option<&str>) -> Self {
        let persist_restore_token = match value {
            None | Some("1" | "true") => true,
            Some("0" | "false") => false,
            Some(value) => {
                eprintln!(
                    "computer-use-mcp: invalid COMPUTER_USE_MCP_PERSIST_PORTAL={value:?}; persistence remains enabled"
                );
                true
            }
        };
        Self {
            persist_restore_token,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalStream {
    pub stream_index: usize,
    pub node_id: u32,
    pub pipewire_serial: Option<u64>,
    pub id: Option<String>,
    pub mapping_id: Option<String>,
    pub position: Option<(i32, i32)>,
    pub logical_size: Option<(i32, i32)>,
}

impl PortalStream {
    pub fn capture_target(&self) -> CaptureTarget {
        CaptureTarget {
            stream_index: self.stream_index,
            node_id: self.node_id,
            pipewire_serial: self.pipewire_serial,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalCapabilities {
    pub remote_desktop_version: u32,
    pub screencast_version: u32,
    pub available_device_types: u32,
    pub available_source_types: u32,
    pub available_cursor_modes: u32,
}

fn validate_start_devices(mask: u32, requested: u32) -> Result<(), String> {
    if mask & !requested != 0 {
        return Err(format!(
            "RemoteDesktop.Start granted devices outside the requested mask (granted={mask}, requested={requested})"
        ));
    }
    if mask & REQUIRED_DEVICES != REQUIRED_DEVICES {
        return Err(format!(
            "KDE approved device mask {mask}, but both keyboard and pointer access are required"
        ));
    }
    Ok(())
}

pub trait PortalBackend: Send + Sync + 'static {
    fn establish(&self) -> impl Future<Output = Result<PortalConnection, String>> + Send + '_;
    fn capabilities(&self) -> impl Future<Output = Result<PortalCapabilities, String>> + Send + '_;
}

#[derive(Debug)]
pub struct XdgPortalBackend {
    token_store: Option<RestoreTokenStore>,
}

impl XdgPortalBackend {
    pub fn new(config: PortalConfig) -> Self {
        let token_store = if config.persist_restore_token {
            match RestoreTokenStore::xdg() {
                Ok(store) => Some(store),
                Err(error) => {
                    eprintln!("computer-use-mcp: restore-token persistence disabled: {error}");
                    None
                }
            }
        } else {
            None
        };
        Self { token_store }
    }

    pub(crate) fn persistent() -> Result<Self, String> {
        Ok(Self {
            token_store: Some(RestoreTokenStore::xdg()?),
        })
    }

    pub(crate) async fn approve(&self) -> Result<PortalApproval, String> {
        require_wayland()?;
        let connection = Connection::session()
            .await
            .map_err(|error| format!("cannot connect to the user D-Bus session: {error}"))?;
        let remote = RemoteDesktop::with_connection(connection.clone())
            .await
            .map_err(portal_error)?;
        let screencast = Screencast::with_connection(connection.clone())
            .await
            .map_err(portal_error)?;
        let capabilities = read_capabilities(&remote, &screencast).await?;
        validate_capabilities(&capabilities)?;

        let session_token = random_token("session")?;
        let session_path = predicted_path(&connection, "session", &session_token)?;
        let session_proxy = Proxy::new_owned(
            connection.clone(),
            PORTAL_DESTINATION,
            session_path.clone(),
            SESSION_INTERFACE,
        )
        .await
        .map_err(portal_error)?;
        let closed_stream = session_proxy
            .receive_signal("Closed")
            .await
            .map_err(portal_error)?;
        let (closed_sender, closed) = watch::channel(false);
        let monitor_closed_sender = closed_sender.clone();
        let close_monitor = CloseMonitor(Some(tokio::spawn(async move {
            let mut stream = closed_stream;
            let signalled = stream.next().await.is_some();
            monitor_closed_sender.send_replace(true);
            if signalled {
                eprintln!("computer-use-mcp: XDG portal RemoteDesktop session closed");
            } else {
                eprintln!(
                    "computer-use-mcp: XDG portal session close monitor ended; invalidating the session"
                );
            }
        })));
        let mut session_guard = CloseGuard::new(session_proxy.clone(), "portal session");

        let request_token = random_token("create")?;
        let mut request = RawRequest::new(&connection, &request_token).await?;
        let mut options = HashMap::new();
        options.insert("handle_token", Value::from(request_token));
        options.insert("session_handle_token", Value::from(session_token));
        let returned: OwnedObjectPath = remote
            .call("CreateSession", &options)
            .await
            .map_err(portal_error)?;
        let mut response = request.response(returned, "CreateSession", &closed).await?;
        let returned_session = take_string(&mut response, "session_handle")?;
        if returned_session != session_path {
            eprintln!(
                "computer-use-mcp: portal returned unexpected session path: expected={session_path} returned={returned_session}"
            );
            if let Ok(returned_proxy) = Proxy::new_owned(
                connection.clone(),
                PORTAL_DESTINATION,
                returned_session.clone(),
                SESSION_INTERFACE,
            )
            .await
            {
                let _ = returned_proxy.call::<_, _, ()>("Close", &()).await;
            }
            return Err(
                "portal returned a session identity that did not match the requested handle".into(),
            );
        }

        let restore_token = if let Some(store) = &self.token_store {
            match store.take() {
                Ok(Some(token)) => {
                    eprintln!("computer-use-mcp: using a private one-shot portal restore token");
                    Some(token)
                }
                Ok(None) => None,
                Err(error) => {
                    eprintln!(
                        "computer-use-mcp: restore token unavailable; continuing without restoration: {error}"
                    );
                    None
                }
            }
        } else {
            None
        };

        select_devices(
            &connection,
            &remote,
            &session_path,
            self.token_store.is_some(),
            restore_token.as_deref(),
            &closed,
        )
        .await?;
        select_sources(
            &connection,
            &screencast,
            &session_path,
            capabilities.available_cursor_modes,
            &closed,
        )
        .await?;
        let mut start_results =
            start_remote_desktop(&connection, &remote, &session_path, &closed).await?;
        let granted_mask = take_owned(&mut start_results, "devices")?
            .try_into()
            .map_err(|error| format!("portal response devices has the wrong type: {error}"))?;
        match validate_start_devices(granted_mask, REQUIRED_DEVICES) {
            Ok(()) => {}
            Err(error) => {
                match session_proxy.call::<_, _, ()>("Close", &()).await {
                    Ok(()) => session_guard.disarm(),
                    Err(close_error) => eprintln!(
                        "computer-use-mcp: failed to close rejected portal grant: {close_error}"
                    ),
                }
                return Err(error);
            }
        };
        let stream = parse_streams(
            take_owned(&mut start_results, "streams")?,
            capabilities.screencast_version,
        )?;
        let mut restore_token_saved = false;
        if let Some(store) = &self.token_store {
            let restore_token = match take_optional::<String>(&mut start_results, "restore_token") {
                Ok(token) => token,
                Err(error) => {
                    eprintln!(
                        "computer-use-mcp: portal session is usable, but its replacement restore token had the wrong type: {error}"
                    );
                    None
                }
            };
            match restore_token {
                Some(token) => match store.save(&token) {
                    Ok(()) => {
                        restore_token_saved = true;
                        eprintln!(
                            "computer-use-mcp: saved a private one-shot portal restore token"
                        );
                    }
                    Err(error) => eprintln!(
                        "computer-use-mcp: portal session is usable, but its replacement restore token could not be saved: {error}"
                    ),
                },
                None => eprintln!(
                    "computer-use-mcp: portal granted the session without a restore token; persistence is unavailable for this backend/choice"
                ),
            }
        }
        let generation = next_session_generation(&SESSION_GENERATION)?;
        let lease = Arc::new(PortalSessionLease {
            path: session_path,
            generation,
            closed,
            closed_sender,
            connection: Some(connection),
            input_state: Mutex::new(EisConnectionState::Available),
            _close_guard: Mutex::new(session_guard),
            _close_monitor: close_monitor,
        });
        Ok(PortalApproval {
            session: lease,
            stream,
            restore_token_saved,
        })
    }

    async fn establish_inner(&self) -> Result<PortalConnection, String> {
        let approval = self.approve().await?;
        let result = async {
            let connection = approval.session.connection()?;
            let screencast = Screencast::with_connection(connection.clone())
                .await
                .map_err(portal_error)?;
            let session = ObjectPath::try_from(approval.session.identity())
                .map_err(|error| format!("invalid portal session path: {error}"))?;
            let options: HashMap<&str, Value<'_>> = HashMap::new();
            let fd: zbus::zvariant::OwnedFd = screencast
                .call("OpenPipeWireRemote", &(session, options))
                .await
                .map_err(portal_error)?;
            Ok::<_, String>(fd)
        }
        .await;
        match result {
            Ok(fd) => Ok(PortalConnection {
                fd: fd.into(),
                session: approval.session,
                stream: approval.stream,
            }),
            Err(error) => {
                if let Err(close) = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    approval.session.close("PipeWire portal startup failed"),
                )
                .await
                .unwrap_or_else(|_| Err("timed out closing failed portal startup".into()))
                {
                    eprintln!("computer-use-mcp: portal startup cleanup also failed: {close}");
                }
                Err(error)
            }
        }
    }
}

impl Default for XdgPortalBackend {
    fn default() -> Self {
        Self::new(PortalConfig::from_env())
    }
}

impl PortalBackend for XdgPortalBackend {
    fn establish(&self) -> impl Future<Output = Result<PortalConnection, String>> + Send + '_ {
        self.establish_inner()
    }

    async fn capabilities(&self) -> Result<PortalCapabilities, String> {
        require_wayland()?;
        let connection = Connection::session().await.map_err(portal_error)?;
        let remote = RemoteDesktop::with_connection(connection.clone())
            .await
            .map_err(portal_error)?;
        let screencast = Screencast::with_connection(connection)
            .await
            .map_err(portal_error)?;
        read_capabilities(&remote, &screencast).await
    }
}

#[derive(Debug)]
pub(crate) struct PortalApproval {
    pub session: Arc<PortalSessionLease>,
    pub stream: PortalStream,
    pub restore_token_saved: bool,
}

#[derive(Debug)]
pub struct PortalConnection {
    pub fd: OwnedFd,
    pub session: Arc<PortalSessionLease>,
    pub stream: PortalStream,
}

pub struct PortalSessionLease {
    path: String,
    generation: u64,
    closed: watch::Receiver<bool>,
    closed_sender: watch::Sender<bool>,
    connection: Option<Connection>,
    input_state: Mutex<EisConnectionState>,
    _close_guard: Mutex<CloseGuard>,
    _close_monitor: CloseMonitor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EisConnectionState {
    Available,
    Connecting,
    Connected,
    Invalid,
}

impl std::fmt::Debug for PortalSessionLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PortalSessionLease")
            .field("path", &self.path)
            .field("generation", &self.generation)
            .field("closed", &*self.closed.borrow())
            .finish()
    }
}

impl PortalSessionLease {
    pub fn identity(&self) -> &str {
        &self.path
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    pub(crate) fn begin_eis_attempt(&self) -> Result<(), String> {
        let mut state = self
            .input_state
            .lock()
            .map_err(|_| "portal EIS state mutex poisoned".to_owned())?;
        match *state {
            EisConnectionState::Available if !self.is_closed() => {
                *state = EisConnectionState::Connecting;
                Ok(())
            }
            EisConnectionState::Available => {
                Err("portal session closed before ConnectToEIS".into())
            }
            EisConnectionState::Connecting | EisConnectionState::Connected => {
                Err("ConnectToEIS is one-shot for this portal session".into())
            }
            EisConnectionState::Invalid => Err("portal EIS session is invalid".into()),
        }
    }

    pub(crate) async fn connect_to_eis(&self) -> Result<UnixStream, String> {
        if *self
            .input_state
            .lock()
            .map_err(|_| "portal EIS state mutex poisoned".to_owned())?
            != EisConnectionState::Connecting
        {
            return Err("ConnectToEIS called without an active one-shot attempt".into());
        }
        let connection = self.connection()?;
        let remote = Proxy::new(
            connection,
            PORTAL_DESTINATION,
            PORTAL_OBJECT_PATH,
            REMOTE_DESKTOP_INTERFACE,
        )
        .await
        .map_err(portal_error)?;
        let session = ObjectPath::try_from(self.path.as_str())
            .map_err(|error| format!("invalid portal session path: {error}"))?;
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        let fd: zbus::zvariant::OwnedFd = remote
            .call("ConnectToEIS", &(session, options))
            .await
            .map_err(portal_error)?;
        Ok(UnixStream::from(OwnedFd::from(fd)))
    }

    pub(crate) fn complete_eis_attempt(&self) -> Result<(), String> {
        let mut state = self
            .input_state
            .lock()
            .map_err(|_| "portal EIS state mutex poisoned".to_owned())?;
        if *state != EisConnectionState::Connecting || self.is_closed() {
            return Err("portal session changed while ConnectToEIS was starting".into());
        }
        *state = EisConnectionState::Connected;
        Ok(())
    }

    pub(crate) fn invalidate(&self, reason: &str) {
        self.invalidate_eis(reason);
        match self._close_guard.lock() {
            Ok(mut guard) => {
                guard.close_now("invalid EIS portal session", self.connection.as_ref())
            }
            Err(_) => eprintln!("computer-use-mcp: portal session close mutex poisoned"),
        }
    }

    pub(crate) fn invalidate_eis(&self, reason: &str) {
        match self.input_state.lock() {
            Ok(mut state) => *state = EisConnectionState::Invalid,
            Err(_) => eprintln!("computer-use-mcp: portal EIS state mutex poisoned"),
        }
        self.closed_sender.send_replace(true);
        eprintln!("computer-use-mcp: invalidating portal session: {reason}");
    }

    pub(crate) async fn close(&self, reason: &str) -> Result<(), String> {
        match self.input_state.lock() {
            Ok(mut state) => *state = EisConnectionState::Invalid,
            Err(_) => return Err("portal EIS state mutex poisoned".into()),
        }
        self.closed_sender.send_replace(true);
        eprintln!("computer-use-mcp: closing portal session: {reason}");
        let proxy = {
            let guard = self
                ._close_guard
                .lock()
                .map_err(|_| "portal session close mutex poisoned".to_owned())?;
            match guard.target.as_ref() {
                Some(CloseTarget::Proxy(proxy)) => Some(proxy.clone()),
                #[cfg(test)]
                Some(CloseTarget::Probe(probe)) => {
                    probe.fetch_add(1, Ordering::AcqRel);
                    None
                }
                None => None,
            }
        };
        if let Some(proxy) = proxy {
            proxy
                .call::<_, _, ()>("Close", &())
                .await
                .map_err(|error| format!("failed to close portal session: {error}"))?;
        }
        self._close_guard
            .lock()
            .map_err(|_| "portal session close mutex poisoned".to_owned())?
            .take();
        Ok(())
    }

    pub(crate) fn connection(&self) -> Result<&Connection, String> {
        self.connection
            .as_ref()
            .ok_or_else(|| "test portal session has no D-Bus connection".to_owned())
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: &str, generation: u64) -> (Arc<Self>, watch::Sender<bool>) {
        let (sender, closed) = watch::channel(false);
        (
            Arc::new(Self {
                path: path.into(),
                generation,
                closed,
                closed_sender: sender.clone(),
                connection: None,
                input_state: Mutex::new(EisConnectionState::Available),
                _close_guard: Mutex::new(CloseGuard::empty("portal session")),
                _close_monitor: CloseMonitor(None),
            }),
            sender,
        )
    }
}

async fn read_capabilities(
    remote: &RemoteDesktop,
    screencast: &Screencast,
) -> Result<PortalCapabilities, String> {
    Ok(PortalCapabilities {
        remote_desktop_version: remote.version(),
        screencast_version: screencast.version(),
        available_device_types: remote
            .get_property("AvailableDeviceTypes")
            .await
            .map_err(portal_error)?,
        available_source_types: screencast
            .get_property("AvailableSourceTypes")
            .await
            .map_err(portal_error)?,
        available_cursor_modes: screencast
            .get_property("AvailableCursorModes")
            .await
            .map_err(portal_error)?,
    })
}

pub fn validate_capabilities(capabilities: &PortalCapabilities) -> Result<(), String> {
    if capabilities.remote_desktop_version < 2 || capabilities.screencast_version < 3 {
        return Err(format!(
            "portal versions are too old: RemoteDesktop={} ScreenCast={} (need RemoteDesktop>=2 and ScreenCast>=3)",
            capabilities.remote_desktop_version, capabilities.screencast_version
        ));
    }
    if capabilities.available_device_types & 3 != 3 {
        return Err(format!(
            "RemoteDesktop portal does not advertise both keyboard and pointer required by EIS input (mask={})",
            capabilities.available_device_types
        ));
    }
    if capabilities.available_source_types & 1 == 0 {
        return Err(format!(
            "ScreenCast portal does not advertise monitor capture (available mask={})",
            capabilities.available_source_types
        ));
    }
    cursor_mode(capabilities.available_cursor_modes)?;
    Ok(())
}

fn cursor_mode(available: u32) -> Result<u32, String> {
    if available & 2 != 0 {
        Ok(2)
    } else if available & 1 != 0 {
        Ok(1)
    } else {
        Err(format!(
            "ScreenCast portal advertises no supported hidden/embedded cursor mode (mask={available})"
        ))
    }
}

async fn select_devices(
    connection: &Connection,
    remote: &RemoteDesktop,
    session_path: &str,
    persist: bool,
    restore_token: Option<&str>,
    closed: &watch::Receiver<bool>,
) -> Result<(), String> {
    let token = random_token("devices")?;
    let mut request = RawRequest::new(connection, &token).await?;
    let mut options = HashMap::new();
    options.insert("handle_token", Value::from(token));
    options.insert("types", Value::from(3_u32));
    if persist {
        options.insert("persist_mode", Value::from(2_u32));
        if let Some(restore_token) = restore_token {
            options.insert("restore_token", Value::from(restore_token));
        }
    }
    let path = ObjectPath::try_from(session_path).map_err(portal_error)?;
    let returned: OwnedObjectPath = remote
        .call("SelectDevices", &(path, options))
        .await
        .map_err(portal_error)?;
    request.response(returned, "SelectDevices", closed).await?;
    Ok(())
}

async fn select_sources(
    connection: &Connection,
    screencast: &Screencast,
    session_path: &str,
    available_cursor_modes: u32,
    closed: &watch::Receiver<bool>,
) -> Result<(), String> {
    let token = random_token("sources")?;
    let mut request = RawRequest::new(connection, &token).await?;
    let mut options = HashMap::new();
    options.insert("handle_token", Value::from(token));
    options.insert("types", Value::from(1_u32));
    options.insert("multiple", Value::from(false));
    options.insert(
        "cursor_mode",
        Value::from(cursor_mode(available_cursor_modes)?),
    );
    let path = ObjectPath::try_from(session_path).map_err(portal_error)?;
    let returned: OwnedObjectPath = screencast
        .call("SelectSources", &(path, options))
        .await
        .map_err(portal_error)?;
    request.response(returned, "SelectSources", closed).await?;
    Ok(())
}

async fn start_remote_desktop(
    connection: &Connection,
    remote: &RemoteDesktop,
    session_path: &str,
    closed: &watch::Receiver<bool>,
) -> Result<HashMap<String, OwnedValue>, String> {
    let token = random_token("start")?;
    let mut request = RawRequest::new(connection, &token).await?;
    let mut options = HashMap::new();
    options.insert("handle_token", Value::from(token));
    let path = ObjectPath::try_from(session_path).map_err(portal_error)?;
    let returned: OwnedObjectPath = remote
        .call("Start", &(path, "", options))
        .await
        .map_err(portal_error)?;
    request.response(returned, "Start", closed).await
}

struct RawRequest {
    expected_path: String,
    proxy: Proxy<'static>,
    stream: MessageStream,
    guard: CloseGuard,
}

impl RawRequest {
    async fn new(connection: &Connection, token: &str) -> Result<Self, String> {
        let expected_path = predicted_path(connection, "request", token)?;
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(PORTAL_DESTINATION)
            .map_err(portal_error)?
            .interface(REQUEST_INTERFACE)
            .map_err(portal_error)?
            .member("Response")
            .map_err(portal_error)?
            .build();
        // Register one generic portal Response match before the method call. It queues both the
        // predicted modern path and any legacy path returned by the method, even if the latter
        // lies outside the modern caller namespace. The response loop filters by returned path.
        let stream = MessageStream::for_match_rule(rule, connection, Some(8))
            .await
            .map_err(portal_error)?;
        let proxy = Proxy::new_owned(
            connection.clone(),
            PORTAL_DESTINATION,
            expected_path.clone(),
            REQUEST_INTERFACE,
        )
        .await
        .map_err(portal_error)?;
        let guard = CloseGuard::new(proxy.clone(), "cancelled portal request");
        Ok(Self {
            expected_path,
            proxy,
            stream,
            guard,
        })
    }

    async fn response(
        &mut self,
        returned: OwnedObjectPath,
        operation: &str,
        closed: &watch::Receiver<bool>,
    ) -> Result<HashMap<String, OwnedValue>, String> {
        if returned.as_str() != self.expected_path {
            eprintln!(
                "computer-use-mcp: portal request path changed for {operation}: expected={} returned={returned}",
                self.expected_path
            );
            self.proxy = Proxy::new_owned(
                self.proxy.connection().clone(),
                PORTAL_DESTINATION,
                returned,
                REQUEST_INTERFACE,
            )
            .await
            .map_err(portal_error)?;
            self.guard.disarm();
            self.guard = CloseGuard::new(self.proxy.clone(), "cancelled portal request");
        }
        let returned_path = self.proxy.path().as_str().to_owned();
        let message =
            wait_for_request_response(&mut self.stream, &returned_path, operation, closed).await?;
        self.guard.disarm();
        let (code, results): (u32, HashMap<String, OwnedValue>) = message
            .body()
            .deserialize()
            .map_err(|error| format!("invalid portal {operation} response: {error}"))?;
        classify_response(code, operation)?;
        Ok(results)
    }
}

async fn wait_for_request_response<S>(
    stream: &mut S,
    returned_path: &str,
    operation: &str,
    closed: &watch::Receiver<bool>,
) -> Result<zbus::Message, String>
where
    S: futures_util::Stream<Item = zbus::Result<zbus::Message>> + Unpin,
{
    let mut closed = closed.clone();
    loop {
        if *closed.borrow() {
            return Err(format!(
                "portal RemoteDesktop session closed during {operation}"
            ));
        }
        tokio::select! {
            message = stream.next() => {
                let message = message
                    .ok_or_else(|| format!("portal {operation} request disappeared without a Response signal"))?
                    .map_err(portal_error)?;
                if message_matches_path(&message, returned_path) {
                    return Ok(message);
                }
            }
            changed = closed.changed() => {
                if changed.is_err() || *closed.borrow() {
                    return Err(format!("portal RemoteDesktop session closed during {operation}"));
                }
                return Err(format!("portal session close monitor changed unexpectedly during {operation}"));
            }
        }
    }
}

fn message_matches_path(message: &zbus::Message, path: &str) -> bool {
    message
        .header()
        .path()
        .is_some_and(|message_path| message_path.as_str() == path)
}

fn classify_response(code: u32, operation: &str) -> Result<(), String> {
    match code {
        0 => Ok(()),
        1 => Err(format!("user cancelled portal {operation} consent")),
        2 => Err(format!("user or portal denied portal {operation} consent")),
        other => Err(format!(
            "portal {operation} returned unknown response code {other}"
        )),
    }
}

enum CloseTarget {
    Proxy(Proxy<'static>),
    #[cfg(test)]
    Probe(Arc<std::sync::atomic::AtomicUsize>),
}

struct CloseGuard {
    target: Option<CloseTarget>,
    label: &'static str,
}

impl CloseGuard {
    fn new(proxy: Proxy<'static>, label: &'static str) -> Self {
        Self {
            target: Some(CloseTarget::Proxy(proxy)),
            label,
        }
    }

    fn disarm(&mut self) {
        self.target = None;
    }

    fn close_now(&mut self, label: &'static str, connection: Option<&Connection>) {
        match self.target.take() {
            Some(CloseTarget::Proxy(proxy)) => close_proxy(proxy, label, connection.cloned()),
            #[cfg(test)]
            Some(CloseTarget::Probe(probe)) => {
                probe.fetch_add(1, Ordering::AcqRel);
            }
            None => {}
        }
    }

    fn take(&mut self) -> Option<CloseTarget> {
        self.target.take()
    }

    #[cfg(test)]
    fn empty(label: &'static str) -> Self {
        Self {
            target: None,
            label,
        }
    }
}

impl std::fmt::Debug for CloseGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloseGuard")
            .field("label", &self.label)
            .field(
                "target",
                &self.target.as_ref().map(|target| match target {
                    CloseTarget::Proxy(proxy) => proxy.path().as_str(),
                    #[cfg(test)]
                    CloseTarget::Probe(_) => "test-probe",
                }),
            )
            .finish()
    }
}

impl Drop for CloseGuard {
    fn drop(&mut self) {
        match self.target.take() {
            Some(CloseTarget::Proxy(proxy)) => close_proxy(proxy, self.label, None),
            #[cfg(test)]
            Some(CloseTarget::Probe(probe)) => {
                probe.fetch_add(1, Ordering::AcqRel);
            }
            None => {}
        }
    }
}

struct CloseMonitor(Option<tokio::task::JoinHandle<()>>);

impl Drop for CloseMonitor {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

fn close_proxy(proxy: Proxy<'static>, label: &'static str, connection: Option<Connection>) {
    let result = std::thread::Builder::new()
        .name("computer-use-mcp-portal-close".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(async move {
                    if let Err(error) = proxy.call::<_, _, ()>("Close", &()).await {
                        eprintln!("computer-use-mcp: failed to close {label}: {error}");
                    }
                    drop(connection);
                }),
                Err(error) => eprintln!(
                    "computer-use-mcp: cannot create cleanup runtime for {label}: {error}"
                ),
            }
        });
    if let Err(error) = result {
        eprintln!("computer-use-mcp: cannot start cleanup thread for {label}: {error}");
    }
}

fn predicted_path(connection: &Connection, kind: &str, token: &str) -> Result<String, String> {
    let sender = sender_path_element(connection)?;
    Ok(format!(
        "/org/freedesktop/portal/desktop/{kind}/{sender}/{token}"
    ))
}

fn sender_path_element(connection: &Connection) -> Result<String, String> {
    Ok(connection
        .unique_name()
        .ok_or_else(|| "D-Bus connection has no unique name".to_owned())?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_"))
}

fn random_token(prefix: &str) -> Result<String, String> {
    let mut random = [0_u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|error| format!("cannot obtain a secure portal request token: {error}"))?;
    let hex = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("computer_use_mcp_{prefix}_{hex}"))
}

fn parse_streams(value: OwnedValue, screencast_version: u32) -> Result<PortalStream, String> {
    let mut streams: Vec<(u32, HashMap<String, OwnedValue>)> = value
        .try_into()
        .map_err(|error| format!("portal returned invalid raw stream metadata: {error}"))?;
    if streams.is_empty() {
        return Err("portal approved no ScreenCast streams".into());
    }
    if streams.len() != 1 {
        return Err(format!(
            "portal approved {} streams; exactly one monitor is required",
            streams.len()
        ));
    }
    let (node_id, properties) = streams.pop().expect("stream count checked");
    parse_stream(0, node_id, properties, screencast_version)
}

fn parse_stream(
    stream_index: usize,
    node_id: u32,
    mut properties: HashMap<String, OwnedValue>,
    screencast_version: u32,
) -> Result<PortalStream, String> {
    let id = take_optional::<String>(&mut properties, "id")?;
    let mapping_id = take_optional::<String>(&mut properties, "mapping_id")?;
    let position = take_optional::<(i32, i32)>(&mut properties, "position")?;
    let logical_size = take_optional::<(i32, i32)>(&mut properties, "size")?;
    let source_raw = take_optional::<u32>(&mut properties, "source_type")?;
    if source_raw != Some(1) {
        return Err(format!(
            "stream {stream_index} is not a monitor source (source_type={source_raw:?})"
        ));
    }
    let pipewire_serial = take_optional::<u64>(&mut properties, "pipewire-serial")?;
    if screencast_version >= 6 && pipewire_serial.is_none() {
        return Err(format!(
            "ScreenCast v{screencast_version} stream {stream_index} omitted required pipewire-serial metadata"
        ));
    }
    if screencast_version < 6 {
        eprintln!(
            "computer-use-mcp: ScreenCast v{screencast_version} lacks stable pipewire-serial targeting; using session-scoped node ID {node_id}"
        );
    }
    if !properties.is_empty() {
        let names = properties.keys().cloned().collect::<Vec<_>>().join(", ");
        eprintln!(
            "computer-use-mcp: ignoring unsupported portal stream properties for stream {stream_index}: {names}"
        );
    }
    Ok(PortalStream {
        stream_index,
        node_id,
        pipewire_serial,
        id,
        mapping_id,
        position,
        logical_size,
    })
}

fn take_owned(values: &mut HashMap<String, OwnedValue>, name: &str) -> Result<OwnedValue, String> {
    values
        .remove(name)
        .ok_or_else(|| format!("portal response omitted {name}"))
}

fn take_string(values: &mut HashMap<String, OwnedValue>, name: &str) -> Result<String, String> {
    take_owned(values, name)?
        .try_into()
        .map_err(|error| format!("portal response {name} has the wrong type: {error}"))
}

fn take_optional<T>(
    values: &mut HashMap<String, OwnedValue>,
    name: &str,
) -> Result<Option<T>, String>
where
    T: TryFrom<OwnedValue>,
    T::Error: std::fmt::Display,
{
    values
        .remove(name)
        .map(|value| {
            value
                .try_into()
                .map_err(|error| format!("portal stream property {name} has wrong type: {error}"))
        })
        .transpose()
}

#[derive(Debug, Clone)]
pub struct RestoreTokenStore {
    directory: PathBuf,
}

impl RestoreTokenStore {
    pub fn xdg() -> Result<Self, String> {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(".local/state"))
            })
            .ok_or_else(|| "neither XDG_STATE_HOME nor HOME is an absolute path".to_owned())?;
        Ok(Self::at(state.join("computer-use-mcp")))
    }

    pub fn at(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn take(&self) -> Result<Option<String>, String> {
        let Some(directory) = open_private_directory(&self.directory, false)? else {
            return Ok(None);
        };
        let claimed = format!(".portal-token-claimed-{}", random_token("file")?);
        match rustix::fs::renameat(
            &directory,
            "portal-restore-token",
            &directory,
            claimed.as_str(),
        ) {
            Ok(()) => {}
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(format!("cannot claim one-shot restore token: {error}")),
        }
        directory
            .sync_all()
            .map_err(|error| format!("cannot sync one-shot restore-token claim: {error}"))?;
        let result = (|| {
            let fd = rustix::fs::openat(
                &directory,
                claimed.as_str(),
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map_err(|error| format!("cannot open claimed restore-token file: {error}"))?;
            let mut file = File::from(fd);
            let metadata = file
                .metadata()
                .map_err(|error| format!("cannot inspect claimed restore-token file: {error}"))?;
            if !metadata.is_file() {
                return Err("restore-token path is not a regular file".into());
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("restore-token file permissions are not private (need 0600)".into());
            }
            if metadata.len() > TOKEN_MAX_BYTES as u64 {
                return Err("restore-token file has invalid content".into());
            }
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            std::io::Read::by_ref(&mut file)
                .take(TOKEN_MAX_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| format!("cannot read restore-token file: {error}"))?;
            if bytes.is_empty() || bytes.len() > TOKEN_MAX_BYTES || bytes.contains(&0) {
                return Err("restore-token file has invalid content".into());
            }
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| "restore-token file is not valid UTF-8".to_owned())
        })();
        rustix::fs::unlinkat(&directory, claimed.as_str(), AtFlags::empty())
            .map_err(|error| format!("cannot remove claimed restore-token file: {error}"))?;
        directory
            .sync_all()
            .map_err(|error| format!("cannot sync restore-token consumption: {error}"))?;
        result
    }

    pub fn save(&self, token: &str) -> Result<(), String> {
        if token.is_empty() || token.len() > TOKEN_MAX_BYTES || token.contains('\0') {
            return Err("portal returned an invalid restore token".into());
        }
        let directory = open_private_directory(&self.directory, true)?
            .ok_or("private restore-token directory was not created")?;
        let temporary = format!(".portal-token-{}", random_token("file")?);
        let result = (|| {
            let fd = rustix::fs::openat(
                &directory,
                temporary.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::from(0o600),
            )
            .map_err(|error| format!("cannot create private restore-token file: {error}"))?;
            let mut file = File::from(fd);
            rustix::fs::fchmod(&file, Mode::from(0o600))
                .map_err(|error| format!("cannot secure restore-token file: {error}"))?;
            file.write_all(token.as_bytes())
                .and_then(|_| file.sync_all())
                .map_err(|error| format!("cannot write private restore-token file: {error}"))?;
            rustix::fs::renameat(
                &directory,
                temporary.as_str(),
                &directory,
                "portal-restore-token",
            )
            .map_err(|error| format!("cannot install private restore-token file: {error}"))?;
            directory
                .sync_all()
                .map_err(|error| format!("cannot sync restore-token directory: {error}"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&directory, temporary.as_str(), AtFlags::empty());
        }
        result
    }

    pub fn invalidate(&self) -> Result<(), String> {
        let Some(directory) = open_private_directory(&self.directory, false)? else {
            return Ok(());
        };
        match rustix::fs::unlinkat(&directory, "portal-restore-token", AtFlags::empty()) {
            Ok(()) => {
                directory
                    .sync_all()
                    .map_err(|error| format!("cannot sync restore-token invalidation: {error}"))?;
                eprintln!("computer-use-mcp: invalidated stored one-shot portal restore token");
                Ok(())
            }
            Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(error) => Err(format!("cannot invalidate restore-token file: {error}")),
        }
    }
}

fn open_private_directory(path: &Path, create: bool) -> Result<Option<File>, String> {
    let missing = matches!(
        fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    if create {
        fs::create_dir_all(path)
            .map_err(|error| format!("cannot create XDG state directory: {error}"))?;
    }
    let fd = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) if !create => return Ok(None),
        Err(error) => return Err(format!("cannot open private XDG state directory: {error}")),
    };
    let directory = File::from(fd);
    if create {
        rustix::fs::fchmod(&directory, Mode::from(0o700))
            .map_err(|error| format!("cannot secure XDG state directory: {error}"))?;
    }
    let metadata = directory
        .metadata()
        .map_err(|error| format!("cannot inspect XDG state directory: {error}"))?;
    if !metadata.is_dir() {
        return Err("XDG state path is not a regular directory".into());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("XDG state directory permissions are not private (need 0700)".into());
    }
    if create && missing {
        let parent = path.parent().ok_or("XDG state directory has no parent")?;
        File::open(parent)
            .and_then(|parent| parent.sync_all())
            .map_err(|error| format!("cannot sync XDG state directory parent: {error}"))?;
    }
    Ok(Some(directory))
}

fn require_wayland() -> Result<(), String> {
    if std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("wayland")
        || std::env::var_os("WAYLAND_DISPLAY").is_none()
    {
        return Err("capture requires the signed-in user's Linux Wayland session".into());
    }
    Ok(())
}

fn portal_error(error: impl std::fmt::Display) -> String {
    format!("XDG portal call failed: {error}")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
        },
    };

    use super::*;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "computer-use-mcp-{name}-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
        ))
    }

    #[test]
    fn portal_persistence_is_enabled_by_default_with_an_explicit_opt_out() {
        assert!(PortalConfig::default().persist_restore_token);
        assert!(PortalConfig::from_persistence_value(None).persist_restore_token);
        assert!(PortalConfig::from_persistence_value(Some("true")).persist_restore_token);
        assert!(!PortalConfig::from_persistence_value(Some("0")).persist_restore_token);
        assert!(PortalConfig::from_persistence_value(Some("invalid")).persist_restore_token);
    }

    #[test]
    fn denial_cancel_and_unknown_response_are_explicit() {
        assert!(classify_response(0, "Start").is_ok());
        assert!(
            classify_response(1, "Start")
                .unwrap_err()
                .contains("cancelled")
        );
        assert!(
            classify_response(2, "Start")
                .unwrap_err()
                .contains("denied")
        );
        assert!(
            classify_response(9, "Start")
                .unwrap_err()
                .contains("unknown")
        );
    }

    #[test]
    fn token_store_enforces_permissions_and_invalid_content() {
        let directory = temp_directory("token");
        let store = RestoreTokenStore::at(directory.clone());
        store.save("secret-token").unwrap();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join("portal-restore-token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(store.take().unwrap().as_deref(), Some("secret-token"));
        assert!(store.take().unwrap().is_none());
        store.save("insecure-token").unwrap();
        fs::set_permissions(
            directory.join("portal-restore-token"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(store.take().unwrap_err().contains("0600"));
        store.invalidate().unwrap();
        store.save("replacement-token\n").unwrap();
        assert_eq!(
            store.take().unwrap().as_deref(),
            Some("replacement-token\n")
        );
        store.invalidate().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn restore_token_is_claimed_by_only_one_process() {
        let directory = temp_directory("token-claim");
        let store = RestoreTokenStore::at(directory.clone());
        store.save("one-shot").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let readers = (0..2)
            .map(|_| {
                let store = store.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.take().unwrap()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let claimed = readers
            .into_iter()
            .filter_map(|reader| reader.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(claimed, ["one-shot"]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn actual_device_grants_are_typed_and_validated() {
        validate_start_devices(3, 3).unwrap();

        assert!(
            validate_start_devices(1, 3)
                .unwrap_err()
                .contains("both keyboard and pointer")
        );
        assert!(validate_start_devices(7, 3).is_err());
        assert!(
            validate_start_devices(4, 3)
                .unwrap_err()
                .contains("outside")
        );
    }

    #[tokio::test]
    async fn generic_request_subscription_can_accept_changed_handle_without_a_gap() {
        let expected = "/org/freedesktop/portal/desktop/request/1_2/expected";
        let returned = "/org/freedesktop/portal/desktop/request/legacy_sender/legacy";
        let expected_message = zbus::Message::signal(expected, REQUEST_INTERFACE, "Response")
            .unwrap()
            .build(&())
            .unwrap();
        let returned_message = zbus::Message::signal(returned, REQUEST_INTERFACE, "Response")
            .unwrap()
            .build(&())
            .unwrap();
        assert!(!message_matches_path(&expected_message, returned));
        assert!(message_matches_path(&returned_message, returned));
        let mut stream = futures_util::stream::iter([Ok(expected_message), Ok(returned_message)]);
        let (_sender, closed) = watch::channel(false);
        let response = wait_for_request_response(&mut stream, returned, "Start", &closed)
            .await
            .unwrap();
        assert!(message_matches_path(&response, returned));
    }

    #[tokio::test]
    async fn session_closed_interrupts_pending_request_response() {
        let (sender, receiver) = watch::channel(false);
        let mut stream = futures_util::stream::pending::<zbus::Result<zbus::Message>>();
        sender.send_replace(true);
        let error = wait_for_request_response(
            &mut stream,
            "/org/freedesktop/portal/desktop/request/1_2/start",
            "Start",
            &receiver,
        )
        .await
        .unwrap_err();
        assert!(error.contains("session closed during Start"));
    }

    #[test]
    fn cancelled_request_guard_closes_once_and_completed_request_does_not_close() {
        let close_count = Arc::new(AtomicUsize::new(0));
        {
            let _guard = CloseGuard {
                target: Some(CloseTarget::Probe(Arc::clone(&close_count))),
                label: "cancelled portal request",
            };
        }
        assert_eq!(close_count.load(AtomicOrdering::Acquire), 1);
        {
            let mut guard = CloseGuard {
                target: Some(CloseTarget::Probe(Arc::clone(&close_count))),
                label: "cancelled portal request",
            };
            guard.disarm();
        }
        assert_eq!(close_count.load(AtomicOrdering::Acquire), 1);
    }

    #[test]
    fn session_cleanup_guard_closes_exactly_once() {
        let close_count = Arc::new(AtomicUsize::new(0));
        {
            let mut guard = CloseGuard {
                target: Some(CloseTarget::Probe(Arc::clone(&close_count))),
                label: "portal session",
            };
            guard.close_now("portal session", None);
        }
        assert_eq!(close_count.load(AtomicOrdering::Acquire), 1);
    }

    #[test]
    fn stream_metadata_parses_v6_mapping_and_serial() {
        let mut properties = HashMap::new();
        properties.insert(
            "position".into(),
            OwnedValue::try_from(Value::from((-1920_i32, 0_i32))).unwrap(),
        );
        properties.insert(
            "size".into(),
            OwnedValue::try_from(Value::from((1920_i32, 1080_i32))).unwrap(),
        );
        properties.insert("source_type".into(), OwnedValue::from(1_u32));
        properties.insert(
            "mapping_id".into(),
            OwnedValue::try_from(Value::from("map-1")).unwrap(),
        );
        properties.insert("pipewire-serial".into(), OwnedValue::from(400_u64));
        properties.insert("future".into(), OwnedValue::from(7_u32));
        let stream = parse_stream(0, 5, properties, 6).unwrap();
        assert_eq!(stream.position, Some((-1920, 0)));
        assert_eq!(stream.mapping_id.as_deref(), Some("map-1"));
        assert_eq!(stream.pipewire_serial, Some(400));
    }

    #[test]
    fn session_generation_is_monotonic_and_does_not_wrap() {
        let counter = AtomicU64::new(0);
        let values = (0..100)
            .map(|_| next_session_generation(&counter).unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(values.len(), 100);

        let exhausted = AtomicU64::new(u64::MAX);
        assert!(next_session_generation(&exhausted).is_err());
        assert_eq!(exhausted.load(AtomicOrdering::Acquire), u64::MAX);
    }
}
