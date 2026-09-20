//! Best-effort KDE virtual-desktop awareness for window activation.
//!
//! Screenshots capture the current monitor only, so a window living on another
//! virtual desktop can never become visible through AT-SPI dispatch alone.
//! This module talks to KWin over the same-user session bus (read the current
//! desktop, move to a target desktop) and maps catalog entries to desktops
//! when the entry actually carries that data.
//!
//! Match quality, recorded honestly: only KDE-rich catalog records populate
//! `virtual_desktops`, and that authority is unavailable on hosts whose KWin
//! does not advertise the plasma window-management protocol (such as this
//! one). AT-SPI exposes no desktop concept at all, so AT-SPI-only entries
//! always locate as [`WindowDesktop::Unknown`]. Every bus failure degrades to
//! [`DesktopProbe::Unavailable`] and every caller fails closed to its previous
//! behavior; pre-dispatch assist attempts at most one desktop switch, and the
//! post-dispatch AT-SPI hunt walks each non-current desktop at most once (plus
//! one restore), so unbounded desktop-hopping is impossible by construction.

use std::{collections::BTreeSet, fmt, future::Future, pin::Pin, sync::Arc, time::Duration};

use tokio::time::timeout;
use zbus::proxy::CacheProperties;

/// One virtual desktop as reported by KWin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopInfo {
    /// Zero-based position from `a(uss)`.
    pub position: i32,
    /// Stable KWin desktop id (UUID string).
    pub id: String,
    /// Human-readable name such as "Desktop 1".
    pub name: String,
}

impl DesktopInfo {
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "position": self.position,
            "id": self.id,
            "name": self.name,
        })
    }
}

/// Point-in-time view of the KWin virtual-desktop layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopSnapshot {
    /// KWin id of the current desktop.
    pub current_id: String,
    /// All known desktops, position order.
    pub desktops: Vec<DesktopInfo>,
    /// Value of the `count` property at read time.
    pub count: u32,
}

impl DesktopSnapshot {
    /// The current desktop record, if it is still in the desktop list.
    pub fn current(&self) -> Option<&DesktopInfo> {
        self.desktops
            .iter()
            .find(|desktop| desktop.id == self.current_id)
    }

    /// Whether `desktop_id` is the current desktop.
    pub fn is_current(&self, desktop_id: &str) -> bool {
        desktop_id == self.current_id
    }

    /// Compact one-line evidence, e.g. `current="Desktop 1" (1/3)`.
    pub fn summary_text(&self) -> String {
        match self.current() {
            Some(desktop) => format!(
                "current=\"{}\" ({}/{})",
                desktop.name,
                desktop.position.saturating_add(1),
                self.count
            ),
            None => format!("current=\"{}\" (unknown/{})", self.current_id, self.count),
        }
    }

    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "current_id": self.current_id,
            "current": self.current().map(DesktopInfo::as_json),
            "desktops": self.desktops.iter().map(DesktopInfo::as_json).collect::<Vec<_>>(),
            "count": self.count,
        })
    }
}

/// Fail-closed bus error: the reason is model-facing evidence, never a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopError {
    pub reason: String,
}

impl DesktopError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for DesktopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "virtual desktop unavailable: {}", self.reason)
    }
}

impl std::error::Error for DesktopError {}

/// Best-effort belief about which desktop(s) a catalog entry lives on.
///
/// Only KDE-rich records carry `virtual_desktops`; that mapping is
/// authoritative when present. AT-SPI exposes no desktop concept, so
/// AT-SPI-only entries are always [`WindowDesktop::Unknown`] even when their
/// states lack `showing`/`visible` (absence of visibility is a hint, never a
/// desktop id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowDesktop {
    /// The window is pinned to every desktop; switching is pointless.
    OnAllDesktops,
    /// Authoritative KWin desktop ids from a KDE-rich catalog record.
    On(Vec<String>),
    /// No desktop data for this entry (e.g. AT-SPI-only).
    Unknown,
}

/// Map a catalog entry's desktop fields to a [`WindowDesktop`] belief.
pub fn locate_window(virtual_desktops: &[String], states: &BTreeSet<String>) -> WindowDesktop {
    if states.contains("on_all_desktops") {
        return WindowDesktop::OnAllDesktops;
    }
    if virtual_desktops.is_empty() {
        WindowDesktop::Unknown
    } else {
        WindowDesktop::On(virtual_desktops.to_vec())
    }
}

/// Decide whether activation should switch desktops first.
///
/// Returns the desktop id to move to, or `None` when no switch is useful:
/// unknown location, pinned windows, and windows already on the current
/// desktop never switch. At most one id is ever returned, so callers cannot
/// hop across desktops.
pub fn switch_target(location: &WindowDesktop, snapshot: &DesktopSnapshot) -> Option<String> {
    match location {
        WindowDesktop::On(ids) => {
            if ids.iter().any(|id| snapshot.is_current(id)) {
                None
            } else {
                ids.first().cloned()
            }
        }
        WindowDesktop::OnAllDesktops | WindowDesktop::Unknown => None,
    }
}

/// Verify a post-switch snapshot actually landed on the requested desktop.
pub fn verify_switch(requested_id: &str, after: &DesktopSnapshot) -> Result<(), DesktopError> {
    if after.is_current(requested_id) {
        Ok(())
    } else {
        Err(DesktopError::new(format!(
            "desktop switch to {requested_id} was not observed (current is {})",
            after.current_id
        )))
    }
}

/// Session-bus contract behind virtual-desktop I/O. The trait keeps unit
/// tests hermetic: production uses [`KWinSessionBus`], tests inject a fake.
pub trait VirtualDesktopBus: fmt::Debug + Send + Sync {
    fn snapshot(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<DesktopSnapshot, DesktopError>> + Send + '_>>;
    fn switch_to(
        &self,
        desktop_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<DesktopSnapshot, DesktopError>> + Send + '_>>;
}

/// Live KWin implementation over the same-user session bus. Read-only except
/// for setting the `current` desktop property when activation explicitly
/// requests a single bounded switch.
#[derive(Debug, Default, Clone, Copy)]
pub struct KWinSessionBus;

const KWIN_SERVICE: &str = "org.kde.KWin";
const VIRTUAL_DESKTOP_MANAGER_PATH: &str = "/VirtualDesktopManager";
const VIRTUAL_DESKTOP_MANAGER_INTERFACE: &str = "org.kde.KWin.VirtualDesktopManager";
const KWIN_BUS_TIMEOUT: Duration = Duration::from_secs(2);

impl KWinSessionBus {
    async fn session_connection() -> Result<zbus::Connection, DesktopError> {
        if !kde_session_available() {
            return Err(DesktopError::new(
                "KWin virtual desktops are unavailable outside a KDE session",
            ));
        }
        let builder = zbus::connection::Builder::session()
            .map_err(|error| DesktopError::new(format!("session bus unavailable: {error}")))?
            .method_timeout(KWIN_BUS_TIMEOUT);
        call_with_timeout("session bus connection", builder.build()).await
    }

    async fn proxy<'a>(connection: &'a zbus::Connection) -> Result<zbus::Proxy<'a>, DesktopError> {
        let builder = zbus::proxy::Builder::<zbus::Proxy<'a>>::new(connection)
            .destination(KWIN_SERVICE)
            .map_err(|error| DesktopError::new(format!("KWin destination failed: {error}")))?
            .path(VIRTUAL_DESKTOP_MANAGER_PATH)
            .map_err(|error| DesktopError::new(format!("KWin object path failed: {error}")))?
            .interface(VIRTUAL_DESKTOP_MANAGER_INTERFACE)
            .map_err(|error| DesktopError::new(format!("KWin interface failed: {error}")))?
            // Every property read must go back to KWin.  A cached `current`
            // value would make post-switch verification report a stale desktop.
            .cache_properties(CacheProperties::No);
        call_with_timeout("KWin proxy", builder.build()).await
    }

    async fn snapshot_impl() -> Result<DesktopSnapshot, DesktopError> {
        let connection = Self::session_connection().await?;
        let proxy = Self::proxy(&connection).await?;
        Self::snapshot_impl_via(&proxy).await
    }

    async fn switch_to_impl(desktop_id: &str) -> Result<DesktopSnapshot, DesktopError> {
        let connection = Self::session_connection().await?;
        let proxy = Self::proxy(&connection).await?;
        call_with_timeout(
            "setting current desktop",
            proxy.set_property("current", desktop_id),
        )
        .await
        .map_err(|error| DesktopError::new(format!("cannot switch desktop: {error}")))?;

        // Reuse the same connection and uncached proxy, but issue fresh reads
        // after the setter.  This is the authoritative post-switch evidence.
        let after = Self::snapshot_impl_via(&proxy).await?;
        verify_switch(desktop_id, &after)?;
        Ok(after)
    }

    async fn snapshot_impl_via(proxy: &zbus::Proxy<'_>) -> Result<DesktopSnapshot, DesktopError> {
        let current_id: String =
            call_with_timeout("reading current desktop", proxy.get_property("current"))
                .await
                .map_err(|error| {
                    DesktopError::new(format!("cannot read current desktop: {error}"))
                })?;
        if current_id.is_empty() {
            return Err(DesktopError::new(
                "KWin reported an empty current desktop id",
            ));
        }
        let desktops: Vec<(u32, String, String)> =
            call_with_timeout("reading desktop list", proxy.get_property("desktops"))
                .await
                .map_err(|error| DesktopError::new(format!("cannot read desktop list: {error}")))?;
        let count: u32 = call_with_timeout("reading desktop count", proxy.get_property("count"))
            .await
            .map_err(|error| DesktopError::new(format!("cannot read desktop count: {error}")))?;
        Ok(DesktopSnapshot {
            current_id,
            desktops: desktops
                .into_iter()
                .map(|(position, id, name)| DesktopInfo {
                    // KWin positions are uint32 (`a(uss)`); the catalog keeps
                    // them as i32 and they never approach the wrap boundary.
                    position: position as i32,
                    id,
                    name,
                })
                .collect(),
            count,
        })
    }
}

async fn call_with_timeout<T, E, F>(operation: &'static str, future: F) -> Result<T, DesktopError>
where
    E: fmt::Display,
    F: Future<Output = Result<T, E>>,
{
    match timeout(KWIN_BUS_TIMEOUT, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(DesktopError::new(format!("{operation} failed: {error}"))),
        Err(_) => Err(DesktopError::new(format!("{operation} timed out"))),
    }
}

fn kde_session_available() -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    desktop
        .split([':', ';'])
        .any(|part| matches!(part.trim().to_ascii_lowercase().as_str(), "kde" | "plasma"))
        || matches!(
            std::env::var("KDE_FULL_SESSION")
                .ok()
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("1" | "true" | "yes")
        )
}

impl VirtualDesktopBus for KWinSessionBus {
    fn snapshot(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<DesktopSnapshot, DesktopError>> + Send + '_>> {
        Box::pin(Self::snapshot_impl())
    }

    fn switch_to(
        &self,
        desktop_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<DesktopSnapshot, DesktopError>> + Send + '_>> {
        // The id is copied so the future owns its request across awaits.
        let requested = desktop_id.to_owned();
        Box::pin(async move { Self::switch_to_impl(&requested).await })
    }
}

/// Per-call desktop belief handed to list/observe/activate formatting.
///
/// `Disabled` means probing is off entirely (unit tests and runtimes that opt
/// out): callers must emit byte-identical output to the pre-desktop behavior.
/// `Unavailable` is the fail-closed bus error with its reason as evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopProbe {
    Disabled,
    Snapshot(DesktopSnapshot),
    Unavailable(String),
}

impl DesktopProbe {
    /// One-line `Virtual desktop: ...` header, or `None` when disabled so
    /// callers keep their previous output byte-identical.
    pub fn header_line(&self) -> Option<String> {
        match self {
            Self::Disabled => None,
            Self::Snapshot(snapshot) => {
                Some(format!("Virtual desktop: {}", snapshot.summary_text()))
            }
            Self::Unavailable(reason) => Some(format!("Virtual desktop: unavailable ({reason})")),
        }
    }

    /// Structured `virtual_desktops` value, or `None` when disabled.
    pub fn as_json(&self) -> Option<serde_json::Value> {
        match self {
            Self::Disabled => None,
            Self::Snapshot(snapshot) => Some(snapshot.as_json()),
            Self::Unavailable(reason) => Some(serde_json::json!({
                "unavailable": true,
                "reason": reason,
            })),
        }
    }
}

/// Provider held by the runtime. Disabled by default so every existing
/// constructor stays hermetic; production opts into the live KWin bus and
/// tests inject a fake.
#[derive(Debug, Default)]
pub struct VirtualDesktopProvider {
    bus: Option<Arc<dyn VirtualDesktopBus>>,
}

impl VirtualDesktopProvider {
    pub fn disabled() -> Self {
        Self { bus: None }
    }

    pub fn live() -> Self {
        Self {
            bus: Some(Arc::new(KWinSessionBus)),
        }
    }

    pub fn custom(bus: Arc<dyn VirtualDesktopBus>) -> Self {
        Self { bus: Some(bus) }
    }

    pub fn enabled(&self) -> bool {
        self.bus.is_some()
    }

    /// `None` when disabled; otherwise the snapshot or its fail-closed error.
    pub async fn snapshot(&self) -> Option<Result<DesktopSnapshot, DesktopError>> {
        match &self.bus {
            None => None,
            Some(bus) => Some(bus.snapshot().await),
        }
    }

    /// `None` when disabled; otherwise the post-switch snapshot (verified) or
    /// its fail-closed error.
    pub async fn switch_to(
        &self,
        desktop_id: &str,
    ) -> Option<Result<DesktopSnapshot, DesktopError>> {
        match &self.bus {
            None => None,
            Some(bus) => Some(bus.switch_to(desktop_id).await),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> DesktopSnapshot {
        DesktopSnapshot {
            current_id: "id-2".into(),
            desktops: vec![
                DesktopInfo {
                    position: 0,
                    id: "id-1".into(),
                    name: "Desktop 1".into(),
                },
                DesktopInfo {
                    position: 1,
                    id: "id-2".into(),
                    name: "Desktop 2".into(),
                },
            ],
            count: 2,
        }
    }

    #[derive(Debug)]
    struct FakeBus {
        snapshot: Result<DesktopSnapshot, DesktopError>,
        switches: std::sync::Mutex<Vec<String>>,
    }

    impl FakeBus {
        fn ok() -> Self {
            Self {
                snapshot: Ok(sample_snapshot()),
                switches: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn failing(reason: &str) -> Self {
            Self {
                snapshot: Err(DesktopError::new(reason)),
                switches: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl VirtualDesktopBus for FakeBus {
        fn snapshot(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<DesktopSnapshot, DesktopError>> + Send + '_>>
        {
            let snapshot = self.snapshot.clone();
            Box::pin(async move { snapshot })
        }

        fn switch_to(
            &self,
            desktop_id: &str,
        ) -> Pin<Box<dyn Future<Output = Result<DesktopSnapshot, DesktopError>> + Send + '_>>
        {
            self.switches
                .lock()
                .expect("fake switch log is readable")
                .push(desktop_id.to_owned());
            let snapshot = self.snapshot.clone();
            let requested = desktop_id.to_owned();
            Box::pin(async move {
                let after = snapshot?;
                verify_switch(&requested, &after)?;
                Ok(after)
            })
        }
    }

    #[test]
    fn snapshot_summary_prefers_current_record() {
        assert_eq!(
            sample_snapshot().summary_text(),
            "current=\"Desktop 2\" (2/2)"
        );
    }

    #[test]
    fn snapshot_summary_falls_back_to_current_id() {
        let mut snapshot = sample_snapshot();
        snapshot.current_id = "missing".into();
        assert_eq!(snapshot.summary_text(), "current=\"missing\" (unknown/2)");
    }

    #[test]
    fn locate_window_is_honest_about_match_quality() {
        let empty: BTreeSet<String> = BTreeSet::new();
        assert_eq!(locate_window(&[], &empty), WindowDesktop::Unknown);
        assert_eq!(
            locate_window(&["id-1".to_owned()], &empty),
            WindowDesktop::On(vec!["id-1".to_owned()])
        );
        let mut pinned = BTreeSet::new();
        pinned.insert("on_all_desktops".into());
        assert_eq!(
            locate_window(&["id-1".to_owned()], &pinned),
            WindowDesktop::OnAllDesktops
        );
    }

    #[test]
    fn switch_target_fires_only_for_known_other_desktops() {
        let snapshot = sample_snapshot();
        // Already current: no switch.
        assert_eq!(
            switch_target(&WindowDesktop::On(vec!["id-2".into()]), &snapshot),
            None
        );
        // Known other desktop: single bounded switch to the first id.
        assert_eq!(
            switch_target(
                &WindowDesktop::On(vec!["id-1".into(), "id-3".into()]),
                &snapshot
            ),
            Some("id-1".to_owned())
        );
        // Unknown location and pinned windows never switch (no hopping).
        assert_eq!(switch_target(&WindowDesktop::Unknown, &snapshot), None);
        assert_eq!(
            switch_target(&WindowDesktop::OnAllDesktops, &snapshot),
            None
        );
        assert_eq!(switch_target(&WindowDesktop::On(vec![]), &snapshot), None);
    }

    #[test]
    fn verify_switch_rejects_unobserved_moves() {
        let snapshot = sample_snapshot();
        assert!(verify_switch("id-2", &snapshot).is_ok());
        let error = verify_switch("id-1", &snapshot).expect_err("must reject");
        assert!(error.reason.contains("id-1"), "{error}");
    }

    #[test]
    fn disabled_probe_keeps_previous_output_shape() {
        let probe = DesktopProbe::Disabled;
        assert_eq!(probe.header_line(), None);
        assert_eq!(probe.as_json(), None);
    }

    #[test]
    fn unavailable_probe_carries_fail_closed_reason() {
        let probe = DesktopProbe::Unavailable("session bus unavailable".into());
        assert_eq!(
            probe.header_line(),
            Some("Virtual desktop: unavailable (session bus unavailable)".into())
        );
        let json = probe.as_json().expect("unavailable probe has JSON");
        assert_eq!(json["unavailable"], serde_json::Value::Bool(true));
    }

    #[tokio::test]
    async fn provider_passthrough_and_switch_log() {
        let bus = Arc::new(FakeBus::ok());
        let provider =
            VirtualDesktopProvider::custom(Arc::clone(&bus) as Arc<dyn VirtualDesktopBus>);
        assert!(provider.enabled());
        let snapshot = provider
            .snapshot()
            .await
            .expect("enabled provider probes")
            .expect("fake snapshot");
        assert_eq!(snapshot.summary_text(), "current=\"Desktop 2\" (2/2)");
        // Switching to the current desktop verifies cleanly.
        assert!(provider.switch_to("id-2").await.expect("enabled").is_ok());
        // Switching elsewhere fails closed with a verification error.
        assert!(provider.switch_to("id-1").await.expect("enabled").is_err());
        assert_eq!(
            *bus.switches.lock().expect("readable"),
            vec!["id-2".to_owned(), "id-1".to_owned()]
        );
    }

    #[test]
    fn snapshot_accessors_cover_fallback_and_json() {
        let snapshot = sample_snapshot();
        assert_eq!(snapshot.current().expect("current").name, "Desktop 2");
        assert!(snapshot.is_current("id-2"));
        assert!(!snapshot.is_current("id-1"));
        assert_eq!(snapshot.as_json()["current_id"], "id-2");
        assert_eq!(snapshot.as_json()["current"]["name"], "Desktop 2");
        assert_eq!(
            DesktopInfo {
                position: 0,
                id: "id-9".into(),
                name: "Other".into()
            }
            .as_json()["id"],
            "id-9"
        );
        assert_eq!(
            DesktopError::new("bus down").to_string(),
            "virtual desktop unavailable: bus down"
        );
        // Unknown current id stays honest instead of guessing.
        let mut missing = sample_snapshot();
        missing.current_id = "gone".into();
        assert_eq!(missing.current(), None);
        assert_eq!(missing.summary_text(), "current=\"gone\" (unknown/2)");
        assert!(missing.as_json()["current"].is_null());
        // A snapshot probe reports its header line.
        let probe = DesktopProbe::Snapshot(sample_snapshot());
        assert_eq!(
            probe.header_line(),
            Some("Virtual desktop: current=\"Desktop 2\" (2/2)".into())
        );
        assert_eq!(probe.as_json().expect("json")["current_id"], "id-2");
    }

    #[test]
    fn locate_prefers_pin_state_and_switch_matches_any_current() {
        // Pinned state wins even with no desktop ids.
        assert_eq!(
            locate_window(&[], &BTreeSet::from(["on_all_desktops".to_owned()])),
            WindowDesktop::OnAllDesktops
        );
        // Any listed id already current suppresses the switch (no hopping).
        let snapshot = sample_snapshot();
        assert_eq!(
            switch_target(
                &WindowDesktop::On(vec!["id-1".into(), "id-2".into()]),
                &snapshot
            ),
            None
        );
    }

    #[tokio::test]
    async fn provider_error_fails_closed() {
        let provider = VirtualDesktopProvider::custom(
            Arc::new(FakeBus::failing("no KWin")) as Arc<dyn VirtualDesktopBus>
        );
        let error = provider
            .snapshot()
            .await
            .expect("enabled provider probes")
            .expect_err("fake failure");
        assert_eq!(error.reason, "no KWin");
        assert!(!VirtualDesktopProvider::disabled().enabled());
        assert_eq!(VirtualDesktopProvider::disabled().snapshot().await, None);
        assert_eq!(
            VirtualDesktopProvider::disabled().switch_to("id-1").await,
            None
        );
    }
}
