//! Window identity and backend capability boundaries.
//!
//! The compositor, AT-SPI, and application registry do not share a reliable
//! join key.  This module consequently keeps backend records separate and only
//! creates a public target after one backend has supplied an explicit identity.
//! Titles, app IDs, PIDs, and geometry are descriptive fields; none is a target
//! key.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use crate::accessibility::{AppInfo, WindowInfo};

pub const KDE_RICH_ENV: &str = "COMPUTER_USE_MCP_KDE_WINDOW_MANAGEMENT";

/// Substrings (case-insensitive) identifying protected system surfaces such as
/// privilege-escalation prompts, permission portals, password askers, screen
/// lockers, and pinentry dialogs. Matched against app ID, title, and resource
/// name. Titles, app IDs, and resource names are descriptive only and never
/// target keys; they are used here solely as a fail-closed refusal signal.
pub const PROTECTED_SURFACE_PATTERNS: &[&str] = &[
    "org.kde.polkit-kde-authentication-agent-1",
    "org.freedesktop.impl.portal.desktop.kde",
    "systemd-ask-password",
    "org.kde.kscreenlocker",
    "pinentry",
];

/// Returns true when any of the descriptive surface fields matches a known
/// protected-surface pattern (substring, ASCII case-insensitive).
pub fn is_protected_surface(
    app_id: Option<&str>,
    title: &str,
    resource_name: Option<&str>,
) -> bool {
    fn matches(value: &str) -> bool {
        PROTECTED_SURFACE_PATTERNS.iter().any(|pattern| {
            value.as_bytes().windows(pattern.len()).any(|candidate| {
                candidate
                    .iter()
                    .zip(pattern.as_bytes())
                    .all(|(byte, expected)| byte.to_ascii_lowercase() == *expected)
            })
        })
    }
    app_id.is_some_and(matches) || matches(title) || resource_name.is_some_and(matches)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BackendKind {
    Atspi,
    ForeignToplevel,
    KdePlasma,
}

impl BackendKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Atspi => "atspi",
            Self::ForeignToplevel => "ext-foreign-toplevel-list-v1",
            Self::KdePlasma => "kde-plasma-window-management",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityState {
    Supported,
    Unsupported { reason: String },
    Unavailable { reason: String },
    Busy { reason: String },
}

impl CapabilityState {
    pub fn supported() -> Self {
        Self::Supported
    }

    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported {
            reason: reason.into(),
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    pub fn busy(reason: impl Into<String>) -> Self {
        Self::Busy {
            reason: reason.into(),
        }
    }

    pub const fn status(&self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported { .. } => "unsupported",
            Self::Unavailable { .. } => "unavailable",
            Self::Busy { .. } => "busy",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Supported => None,
            Self::Unsupported { reason } | Self::Unavailable { reason } | Self::Busy { reason } => {
                Some(reason)
            }
        }
    }

    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Supported)
    }

    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.status(),
            "reason": self.reason(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowCapabilities {
    pub screenshot: CapabilityState,
    pub accessibility: CapabilityState,
    pub activate: CapabilityState,
}

impl WindowCapabilities {
    pub fn atspi() -> Self {
        Self {
            screenshot: CapabilityState::supported(),
            accessibility: CapabilityState::supported(),
            activate: CapabilityState::supported(),
        }
    }

    pub fn foreign_toplevel() -> Self {
        Self {
            // Capture is the approved complete-monitor portal stream, not a
            // foreign-toplevel source. It can therefore support visual-only
            // observations while remaining unable to prove window cropping.
            screenshot: CapabilityState::supported(),
            accessibility: CapabilityState::unsupported(
                "the standard foreign-toplevel protocol has no accessibility tree",
            ),
            activate: CapabilityState::unsupported(
                "the standard foreign-toplevel protocol is read-only",
            ),
        }
    }

    pub fn kde_rich() -> Self {
        Self {
            // The approved stream is a complete monitor; Plasma geometry is
            // deliberately retained as diagnostics and never used to crop or
            // map pixels.
            screenshot: CapabilityState::supported(),
            accessibility: CapabilityState::unsupported(
                "KDE window management does not provide an accessibility tree",
            ),
            activate: CapabilityState::supported(),
        }
    }

    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "screenshot": self.screenshot.as_json(),
            "accessibility": self.accessibility.as_json(),
            "activate": self.activate.as_json(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendWindow {
    /// Stable only for the lifetime of the backend record. It is never exposed
    /// as a client selector; the catalog turns it into a process-lifetime ID.
    pub backend_identity: String,
    /// An explicit application identity supplied by this backend. A title or
    /// app ID must never be used as a substitute.
    pub application_identity: String,
    pub title: String,
    pub app_id: Option<String>,
    pub pid: Option<u32>,
    pub states: BTreeSet<String>,
    pub outputs: Vec<String>,
    pub virtual_desktops: Vec<String>,
    pub resource_name: Option<String>,
    pub geometry: Option<WindowGeometry>,
    pub source: BackendKind,
    pub capabilities: WindowCapabilities,
    pub atspi: Option<AtspiBinding>,
    /// True when descriptive surface fields match a known protected-surface
    /// pattern (polkit, sudo/password askers, screen locker, pinentry).
    pub is_protected_surface: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtspiBinding {
    pub app: AppInfo,
    pub window: WindowInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub client: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WindowTarget {
    pub app_instance_id: String,
    pub window_instance_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEntry {
    pub target: WindowTarget,
    /// Backend-local identity used only for command routing. It is never
    /// accepted as a public selector.
    pub backend_identity: String,
    pub title: String,
    pub app_id: Option<String>,
    pub pid: Option<u32>,
    pub states: BTreeSet<String>,
    pub outputs: Vec<String>,
    pub virtual_desktops: Vec<String>,
    pub resource_name: Option<String>,
    pub geometry: Option<WindowGeometry>,
    pub source: BackendKind,
    pub capabilities: WindowCapabilities,
    pub atspi: Option<AtspiBinding>,
    /// Mirrors the backend record flag; mutations targeting such entries are
    /// refused before dispatch with `ProtectedSurfaceRefused`.
    pub is_protected_surface: bool,
}

impl WindowEntry {
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "target": {
                "app_instance_id": self.target.app_instance_id,
                "window_instance_id": self.target.window_instance_id,
            },
            "app_instance_id": self.target.app_instance_id,
            "window_instance_id": self.target.window_instance_id,
            "pid": self.pid,
            "title": self.title,
            "app_id": self.app_id,
            "state": self.states.iter().collect::<Vec<_>>(),
            "outputs": self.outputs,
            "virtual_desktops": self.virtual_desktops,
            "resource_name": self.resource_name,
            "geometry": self.geometry.map(|geometry| serde_json::json!({
                "x": geometry.x,
                "y": geometry.y,
                "width": geometry.width,
                "height": geometry.height,
                "space": if geometry.client { "kde_client_logical" } else { "kde_window_logical" },
                "authoritative_for_pixels": false,
            })),
            "source": {
                "authority": self.source.as_str(),
                "kind": self.source.as_str(),
            },
            "capabilities": self.capabilities.as_json(),
            "is_protected_surface": self.is_protected_surface,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    InvalidIdentity(String),
    StaleTarget(WindowTarget),
    GenerationExhausted,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity(identity) => {
                write!(formatter, "backend returned empty identity {identity:?}")
            }
            Self::StaleTarget(target) => write!(
                formatter,
                "window target is stale: app_instance_id={} window_instance_id={}",
                target.app_instance_id, target.window_instance_id
            ),
            Self::GenerationExhausted => {
                formatter.write_str("window catalog ID generation exhausted")
            }
        }
    }
}

impl std::error::Error for CatalogError {}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BackendKey {
    source: BackendKind,
    identity: String,
}

#[derive(Debug, Default)]
pub struct WindowCatalog {
    membership_generation: u64,
    next_id: u64,
    active_apps: BTreeMap<BackendKey, ActiveApp>,
    active_windows: BTreeMap<BackendKey, ActiveWindow>,
    entries: BTreeMap<WindowTarget, WindowEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveApp {
    id: String,
    pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveWindow {
    id: String,
    app_id: String,
    pid: Option<u32>,
}

impl WindowCatalog {
    pub fn membership_generation(&self) -> u64 {
        self.membership_generation
    }

    pub fn reconcile(
        &mut self,
        records: impl IntoIterator<Item = BackendWindow>,
    ) -> Result<Vec<WindowEntry>, CatalogError> {
        // Build the next catalog off to the side. A malformed or duplicate
        // backend record must not discard the last known-good catalog or leave
        // a partially reconciled generation visible to callers.
        let old_apps = &self.active_apps;
        let old_windows = &self.active_windows;
        let old_entries = &self.entries;
        let mut next_id = self.next_id;
        let mut active_apps = BTreeMap::new();
        let mut active_windows = BTreeMap::new();
        let mut next_entries = BTreeMap::new();
        let mut entries = Vec::new();

        for record in records {
            if record.backend_identity.is_empty() || record.application_identity.is_empty() {
                eprintln!(
                    "computer-use-mcp: refusing a window record without explicit identity: source={:?}",
                    record.source
                );
                return Err(CatalogError::InvalidIdentity(record.backend_identity));
            }
            let app_key = BackendKey {
                source: record.source,
                identity: record.application_identity.clone(),
            };
            let window_key = BackendKey {
                source: record.source,
                identity: record.backend_identity.clone(),
            };
            if active_windows.contains_key(&window_key) {
                eprintln!(
                    "computer-use-mcp: refusing duplicate backend window identity: source={:?} identity={:?}",
                    record.source, record.backend_identity
                );
                return Err(CatalogError::InvalidIdentity(record.backend_identity));
            }
            let app_instance_id = active_apps
                .get(&app_key)
                .or_else(|| old_apps.get(&app_key))
                .filter(|active| active.pid == record.pid)
                .map(|active| active.id.clone())
                .map_or_else(
                    || next_id_value("app", &mut next_id),
                    Result::<_, CatalogError>::Ok,
                )?;
            let window_instance_id = old_windows
                .get(&window_key)
                .filter(|active| active.pid == record.pid && active.app_id == app_instance_id)
                .map(|active| active.id.clone())
                .map_or_else(
                    || next_id_value("win", &mut next_id),
                    Result::<_, CatalogError>::Ok,
                )?;
            active_apps.insert(
                app_key,
                ActiveApp {
                    id: app_instance_id.clone(),
                    pid: record.pid,
                },
            );
            active_windows.insert(
                window_key,
                ActiveWindow {
                    id: window_instance_id.clone(),
                    app_id: app_instance_id.clone(),
                    pid: record.pid,
                },
            );
            // Recompute from descriptive fields so a record built without the
            // flag is still refused; OR with the supplied flag for backends
            // that already classified the surface.
            let is_protected_surface = record.is_protected_surface
                || is_protected_surface(
                    record.app_id.as_deref(),
                    record.title.as_str(),
                    record.resource_name.as_deref(),
                );
            let entry = WindowEntry {
                target: WindowTarget {
                    app_instance_id,
                    window_instance_id,
                },
                backend_identity: record.backend_identity,
                title: record.title,
                app_id: record.app_id,
                pid: record.pid,
                states: record.states,
                outputs: record.outputs,
                virtual_desktops: record.virtual_desktops,
                resource_name: record.resource_name,
                geometry: record.geometry,
                source: record.source,
                capabilities: record.capabilities,
                atspi: record.atspi,
                is_protected_surface,
            };
            next_entries.insert(entry.target.clone(), entry.clone());
            entries.push(entry);
        }
        // Pagination order is identity-only. Mutable titles and state must not
        // reshuffle pages or invalidate an otherwise exact cursor.
        entries.sort_by(|left, right| left.target.cmp(&right.target));

        let membership_changed = old_entries.keys().ne(next_entries.keys());
        let next_generation = if membership_changed {
            Some(
                self.membership_generation
                    .checked_add(1)
                    .ok_or(CatalogError::GenerationExhausted)?,
            )
        } else {
            None
        };
        self.active_apps = active_apps;
        self.active_windows = active_windows;
        self.entries = next_entries;
        self.next_id = next_id;
        if let Some(next_generation) = next_generation {
            self.membership_generation = next_generation;
        }
        Ok(entries)
    }

    pub fn reconcile_sources(
        &mut self,
        apps: &[AppInfo],
        compositor: impl IntoIterator<Item = BackendWindow>,
    ) -> Result<Vec<WindowEntry>, CatalogError> {
        let atspi = apps.iter().flat_map(|app| {
            app.windows.iter().map(|window| BackendWindow {
                backend_identity: format!("{}{}", window.object.bus_name, window.object.path),
                application_identity: format!("{}{}", app.object.bus_name, app.object.path),
                title: window.title.clone(),
                app_id: None,
                pid: Some(app.pid),
                states: window.states.clone(),
                outputs: Vec::new(),
                virtual_desktops: Vec::new(),
                resource_name: None,
                geometry: None,
                source: BackendKind::Atspi,
                capabilities: WindowCapabilities::atspi(),
                atspi: Some(AtspiBinding {
                    app: app.clone(),
                    window: window.clone(),
                }),
                is_protected_surface: is_protected_surface(
                    Some(&app.name),
                    window.title.as_str(),
                    None,
                ),
            })
        });
        self.reconcile(atspi.chain(compositor))
    }

    pub fn get(&self, target: &WindowTarget) -> Result<WindowEntry, CatalogError> {
        self.entries
            .get(target)
            .cloned()
            .ok_or_else(|| CatalogError::StaleTarget(target.clone()))
    }

    /// Borrow all live entries. Used by catalog-scoped waits that match on
    /// backend-reported identity rather than an exact opaque target.
    pub fn entries(&self) -> impl Iterator<Item = &WindowEntry> {
        self.entries.values()
    }
}

fn next_id_value(prefix: &str, next_id: &mut u64) -> Result<String, CatalogError> {
    let id = *next_id;
    *next_id = next_id
        .checked_add(1)
        .ok_or(CatalogError::GenerationExhausted)?;
    Ok(format!("{prefix}-{id:016x}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendStatus {
    Supported,
    Unsupported(String),
    Unavailable(String),
    Busy(String),
}

impl BackendStatus {
    pub fn capability(&self) -> CapabilityState {
        match self {
            Self::Supported => CapabilityState::supported(),
            Self::Unsupported(reason) => CapabilityState::unsupported(reason),
            Self::Unavailable(reason) => CapabilityState::unavailable(reason),
            Self::Busy(reason) => CapabilityState::busy(reason),
        }
    }

    pub const fn is_supported(&self) -> bool {
        matches!(self, Self::Supported)
    }
}

pub fn kde_rich_enabled() -> bool {
    match std::env::var(KDE_RICH_ENV).ok().as_deref() {
        Some("1" | "true" | "yes") => true,
        Some("0" | "false" | "no") | None => false,
        Some(value) => {
            eprintln!(
                "computer-use-mcp: invalid {KDE_RICH_ENV}={value:?}; KDE rich window management remains disabled"
            );
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    Unsupported(String),
    Unavailable(String),
    Busy(String),
    Stale(String),
    Unknown(String),
    Failed(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(reason) => write!(formatter, "unsupported: {reason}"),
            Self::Unavailable(reason) => write!(formatter, "unavailable: {reason}"),
            Self::Busy(reason) => write!(formatter, "busy: {reason}"),
            Self::Stale(reason) => write!(formatter, "stale: {reason}"),
            Self::Unknown(reason) => write!(formatter, "unknown outcome: {reason}"),
            Self::Failed(reason) => write!(formatter, "backend failure: {reason}"),
        }
    }
}

impl std::error::Error for BackendError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(source: BackendKind, app: &str, window: &str, title: &str) -> BackendWindow {
        BackendWindow {
            backend_identity: window.into(),
            application_identity: app.into(),
            title: title.into(),
            app_id: Some("org.example.App".into()),
            pid: Some(42),
            states: ["active".into()].into_iter().collect(),
            outputs: vec!["DP-1".into()],
            virtual_desktops: Vec::new(),
            resource_name: None,
            geometry: None,
            source,
            capabilities: if source == BackendKind::Atspi {
                WindowCapabilities::atspi()
            } else {
                WindowCapabilities::foreign_toplevel()
            },
            atspi: None,
            is_protected_surface: false,
        }
    }

    #[test]
    fn protected_surfaces_match_known_patterns_case_insensitively() {
        assert!(is_protected_surface(
            Some("org.kde.polkit-kde-authentication-agent-1"),
            "Authentication",
            None,
        ));
        assert!(is_protected_surface(
            Some("org.freedesktop.impl.portal.desktop.kde"),
            "Permission",
            None,
        ));
        assert!(is_protected_surface(
            None,
            "systemd-ask-password prompt",
            None,
        ));
        assert!(is_protected_surface(
            None,
            "Screen locker",
            Some("org.kde.kscreenlocker"),
        ));
        assert!(is_protected_surface(None, "PINENTRY dialog", None));
        assert!(is_protected_surface(
            Some("ORG.KDE.POLKIT-KDE-AUTHENTICATION-AGENT-1"),
            "x",
            None,
        ));
        assert!(!is_protected_surface(
            Some("org.example.App"),
            "Ordinary window",
            None,
        ));
        assert!(!is_protected_surface(None, "", None));
    }

    #[test]
    fn protected_flag_propagates_through_reconcile_and_serializes() {
        let mut catalog = WindowCatalog::default();
        let mut polkit = record(
            BackendKind::ForeignToplevel,
            "app-polkit",
            "win-polkit",
            "Authentication Required",
        );
        polkit.app_id = Some("org.kde.polkit-kde-authentication-agent-1".into());
        // Leave the stored flag false: reconcile must recompute it from the
        // descriptive fields rather than trusting the record.
        let entries = catalog.reconcile([polkit]).unwrap();
        assert!(entries[0].is_protected_surface);
        let json = entries[0].as_json();
        assert_eq!(json["is_protected_surface"], serde_json::Value::Bool(true));

        let ordinary = catalog
            .reconcile([record(
                BackendKind::ForeignToplevel,
                "app-plain",
                "win-plain",
                "Editor",
            )])
            .unwrap();
        assert!(!ordinary[0].is_protected_surface);
        assert_eq!(
            ordinary[0].as_json()["is_protected_surface"],
            serde_json::Value::Bool(false)
        );
    }

    #[test]
    fn catalog_ids_are_opaque_process_lifetime_and_not_fuzzy_joined() {
        let mut catalog = WindowCatalog::default();
        let first = catalog
            .reconcile([record(BackendKind::Atspi, "app-a", "win-a", "Same")])
            .unwrap();
        let target = first[0].target.clone();
        assert!(target.app_instance_id.starts_with("app-"));
        assert!(target.window_instance_id.starts_with("win-"));

        let second = catalog
            .reconcile([record(
                BackendKind::ForeignToplevel,
                "app-a",
                "win-a",
                "Same",
            )])
            .unwrap();
        assert_ne!(
            target, second[0].target,
            "backend identities must not be joined by names"
        );
        assert!(catalog.get(&target).is_err());

        let third = catalog
            .reconcile([record(
                BackendKind::ForeignToplevel,
                "app-a",
                "win-a",
                "Renamed",
            )])
            .unwrap();
        assert_eq!(second[0].target, third[0].target);
        assert_eq!(third[0].title, "Renamed");
    }

    #[test]
    fn lifecycle_reappearance_gets_a_new_process_lifetime_id() {
        let mut catalog = WindowCatalog::default();
        let first = catalog
            .reconcile([record(BackendKind::Atspi, "app-a", "win-a", "Window")])
            .unwrap()[0]
            .target
            .clone();
        catalog.reconcile(std::iter::empty()).unwrap();
        let second = catalog
            .reconcile([record(BackendKind::Atspi, "app-a", "win-a", "Window")])
            .unwrap()[0]
            .target
            .clone();
        assert_ne!(first, second);
        assert!(catalog.get(&first).is_err());
    }

    #[test]
    fn duplicate_records_fail_without_committing_a_partial_generation() {
        let mut catalog = WindowCatalog::default();
        let first = catalog
            .reconcile([record(BackendKind::Atspi, "app-a", "win-a", "Window")])
            .unwrap()[0]
            .target
            .clone();
        let generation = catalog.membership_generation();

        let error = catalog
            .reconcile([
                record(BackendKind::Atspi, "app-a", "win-a", "Window"),
                record(BackendKind::Atspi, "app-a", "win-a", "Duplicate"),
            ])
            .unwrap_err();

        assert!(matches!(error, CatalogError::InvalidIdentity(identity) if identity == "win-a"));
        assert_eq!(catalog.membership_generation(), generation);
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.get(&first).unwrap().title, "Window");
    }

    #[test]
    fn multiple_windows_keep_one_exact_application_instance() {
        let mut catalog = WindowCatalog::default();
        let entries = catalog
            .reconcile([
                record(BackendKind::Atspi, "app-a", "win-a", "First"),
                record(BackendKind::Atspi, "app-a", "win-b", "Second"),
            ])
            .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].target.app_instance_id,
            entries[1].target.app_instance_id
        );
        assert_ne!(
            entries[0].target.window_instance_id,
            entries[1].target.window_instance_id
        );
    }

    #[test]
    fn mutable_metadata_does_not_change_pagination_membership() {
        let mut catalog = WindowCatalog::default();
        let first = catalog
            .reconcile([
                record(BackendKind::Atspi, "app-a", "win-a", "Zulu"),
                record(BackendKind::Atspi, "app-b", "win-b", "Alpha"),
            ])
            .unwrap();
        let generation = catalog.membership_generation();
        let targets = first
            .iter()
            .map(|entry| entry.target.clone())
            .collect::<Vec<_>>();

        let mut updated_a = record(BackendKind::Atspi, "app-a", "win-a", "Aardvark");
        updated_a.states.clear();
        let mut updated_b = record(BackendKind::Atspi, "app-b", "win-b", "Zebra");
        updated_b.states.insert("showing".into());
        let second = catalog.reconcile([updated_a, updated_b]).unwrap();

        assert_eq!(catalog.membership_generation(), generation);
        assert_eq!(
            second
                .iter()
                .map(|entry| entry.target.clone())
                .collect::<Vec<_>>(),
            targets
        );
        assert_eq!(second[0].title, "Aardvark");
        assert_eq!(second[1].title, "Zebra");
    }

    #[test]
    fn membership_change_invalidates_pagination_generation() {
        let mut catalog = WindowCatalog::default();
        catalog
            .reconcile([record(BackendKind::Atspi, "app-a", "win-a", "First")])
            .unwrap();
        let generation = catalog.membership_generation();

        catalog
            .reconcile([
                record(BackendKind::Atspi, "app-a", "win-a", "First"),
                record(BackendKind::Atspi, "app-b", "win-b", "Second"),
            ])
            .unwrap();

        assert_ne!(catalog.membership_generation(), generation);
    }

    #[test]
    fn pid_replacement_gets_new_application_and_window_ids() {
        let mut catalog = WindowCatalog::default();
        let first = catalog
            .reconcile([record(BackendKind::Atspi, "app-a", "win-a", "Window")])
            .unwrap()[0]
            .target
            .clone();
        let mut replacement = record(BackendKind::Atspi, "app-a", "win-a", "Window");
        replacement.pid = Some(43);

        let second = catalog.reconcile([replacement]).unwrap()[0].target.clone();

        assert_ne!(first.app_instance_id, second.app_instance_id);
        assert_ne!(first.window_instance_id, second.window_instance_id);
        assert!(catalog.get(&first).is_err());
    }

    #[test]
    fn capability_states_are_explicit() {
        let status = CapabilityState::busy("single-client conflict");
        assert_eq!(status.status(), "busy");
        assert_eq!(status.reason(), Some("single-client conflict"));
        assert_eq!(
            WindowCapabilities::foreign_toplevel().activate.status(),
            "unsupported"
        );
        assert_eq!(
            WindowCapabilities::kde_rich().activate.status(),
            "supported"
        );
    }
}
