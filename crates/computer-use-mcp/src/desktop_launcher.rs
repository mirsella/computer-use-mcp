use std::{
    fs::OpenOptions,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gio::{
    AppInfo, AppLaunchContext, DesktopAppInfo,
    glib::SpawnFlags,
    prelude::{AppInfoExt, Cast, DesktopAppInfoExtManual},
};

use crate::errors::{RuntimeError, ToolOutcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    pub desktop_id: String,
    pub name: String,
    pub shown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchResult {
    pub desktop_id: String,
    pub name: String,
}

struct LaunchReset(Arc<AtomicBool>);

impl Drop for LaunchReset {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct DesktopEntry {
    info: InstalledApp,
    app: DesktopAppInfo,
}

pub async fn list_installed_apps() -> Result<Vec<InstalledApp>, RuntimeError> {
    tokio::task::spawn_blocking(|| {
        let mut apps = installed_entries()
            .map(|entry| entry.info)
            .collect::<Vec<_>>();
        apps.sort_by_cached_key(|app| {
            (
                app.name.to_lowercase(),
                app.name.clone(),
                app.desktop_id.clone(),
            )
        });
        Ok(apps)
    })
    .await
    .map_err(|error| backend_error(format!("installed app listing task failed: {error}")))?
}

pub async fn launch(
    desktop_id: &str,
    in_progress: Arc<AtomicBool>,
) -> Result<LaunchResult, RuntimeError> {
    in_progress
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| launch_in_progress_error())?;
    let reset = LaunchReset(in_progress);
    let desktop_id = desktop_id.to_owned();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("computer-use-mcp-launch".into())
        .spawn(move || {
            let result = launch_blocking(&desktop_id);
            drop(reset);
            let _ = sender.send(result);
        })
        .map_err(|error| {
            backend_error(format!("cannot start desktop app launch thread: {error}"))
        })?;
    receiver
        .await
        .map_err(|_| backend_error("desktop app launch thread stopped without a result"))?
}

fn installed_entries() -> impl Iterator<Item = DesktopEntry> {
    AppInfo::all()
        .into_iter()
        .filter_map(|app| app.downcast::<DesktopAppInfo>().ok())
        .filter_map(|app| {
            let desktop_id = app.id()?.to_string();
            Some(DesktopEntry {
                info: InstalledApp {
                    desktop_id,
                    name: app.name().to_string(),
                    shown: app.should_show(),
                },
                app,
            })
        })
}

fn launch_blocking(desktop_id: &str) -> Result<LaunchResult, RuntimeError> {
    let DesktopEntry { info, app } = installed_entries()
        .find(|entry| entry.info.desktop_id == desktop_id)
        .ok_or_else(|| {
            RuntimeError::new(
                "target_unavailable",
                format!("installed desktop application not found: {desktop_id:?}"),
                ToolOutcome::NotStarted,
                false,
                "Call list_applications with scope=installed and use an exact returned desktop_id.",
            )
        })?;
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|error| backend_error(format!("cannot open /dev/null for app launch: {error}")))?;
    app.launch_uris_as_manager_with_fds::<AppLaunchContext>(
        &[],
        None,
        SpawnFlags::SEARCH_PATH,
        None,
        None,
        Some(&null),
        Some(&null),
        Some(&null),
    )
    .map_err(|error| {
        RuntimeError::new(
            "backend_failed",
            format!("failed to launch desktop app: {error}"),
            ToolOutcome::Unknown,
            false,
            "Inspect running applications before deciding whether to launch again.",
        )
    })?;
    Ok(LaunchResult {
        desktop_id: info.desktop_id,
        name: info.name,
    })
}

fn launch_in_progress_error() -> RuntimeError {
    RuntimeError::new(
        "backend_failed",
        "a desktop application launch is still in progress",
        ToolOutcome::NotStarted,
        true,
        "Wait for the launch to finish, then observe the target or retry.",
    )
}

fn backend_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        "backend_failed",
        message,
        ToolOutcome::NotStarted,
        true,
        "Retry once. If the failure persists, inspect server diagnostics.",
    )
}
