use std::{
    fs::OpenOptions,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use gio::{
    AppInfo, AppLaunchContext, DesktopAppInfo,
    glib::SpawnFlags,
    prelude::{AppInfoExt, Cast, DesktopAppInfoExtManual},
};

use crate::errors::{RuntimeError, ToolOutcome};
use crate::runtime::ActionProgress;

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

#[derive(Debug, Default)]
pub struct LaunchTasks {
    task: std::sync::Mutex<Option<LaunchTask>>,
}

#[derive(Debug)]
struct LaunchTask {
    cancel: Arc<AtomicBool>,
    join: JoinHandle<()>,
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
    tasks: Arc<LaunchTasks>,
    progress: Arc<ActionProgress>,
) -> Result<LaunchResult, RuntimeError> {
    in_progress
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| launch_in_progress_error())?;
    let reset = LaunchReset(Arc::clone(&in_progress));
    let in_progress_for_join = Arc::clone(&in_progress);
    let desktop_id = desktop_id.to_owned();
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    let thread_progress = Arc::clone(&progress);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("computer-use-mcp-launch".into())
        .spawn(move || {
            let _reset = reset;
            let result = launch_blocking(&desktop_id, &thread_cancel, &thread_progress);
            let _ = sender.send(result);
        })
        .map_err(|error| {
            in_progress.store(false, Ordering::Release);
            backend_error(format!("cannot start desktop app launch thread: {error}"))
        })?;
    tasks
        .task
        .lock()
        .map_err(|_| backend_error("desktop launch task state mutex poisoned"))?
        .replace(LaunchTask {
            cancel,
            join: thread,
        });
    let result = receiver
        .await
        .map_err(|_| backend_error("desktop app launch thread stopped without a result"));
    let joined = join_task(&tasks, in_progress_for_join, Duration::from_secs(1)).await;
    if joined.is_err() {
        progress.mark_cleanup_failed();
    }
    match (result, joined) {
        (Ok(result), Ok(())) => result,
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(result), Err(join)) => Err(backend_error(format!(
            "desktop app launch failed: {result}; joining its thread also failed: {join}"
        ))),
    }
}

pub async fn cancel_and_join(
    in_progress: Arc<AtomicBool>,
    tasks: Arc<LaunchTasks>,
    timeout: Duration,
) -> Result<(), RuntimeError> {
    join_launch_task(&tasks, in_progress, timeout, true).await
}

async fn join_task(
    tasks: &LaunchTasks,
    in_progress: Arc<AtomicBool>,
    timeout: Duration,
) -> Result<(), RuntimeError> {
    join_launch_task(tasks, in_progress, timeout, false).await
}

async fn join_launch_task(
    tasks: &LaunchTasks,
    in_progress: Arc<AtomicBool>,
    timeout: Duration,
    cancel: bool,
) -> Result<(), RuntimeError> {
    let task = tasks
        .task
        .lock()
        .map_err(|_| backend_error("desktop launch task state mutex poisoned"))?
        .take();
    let Some(task) = task else {
        if !cancel {
            in_progress.store(false, Ordering::Release);
        }
        return Ok(());
    };
    if cancel {
        task.cancel.store(true, Ordering::Release);
    }
    let join = tokio::task::spawn_blocking(move || task.join.join());
    match tokio::time::timeout(timeout, join).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(_))) => Err(backend_error("desktop app launch thread panicked")),
        Ok(Err(error)) => Err(backend_error(format!(
            "desktop app launch join task failed: {error}"
        ))),
        Err(_) if cancel => Err(RuntimeError::new(
            "backend_timeout",
            "desktop app launch thread did not join after cancellation",
            ToolOutcome::Unknown,
            false,
            "The launch side effect may have happened. Observe the current desktop before retrying.",
        )),
        Err(_) => Err(RuntimeError::new(
            "backend_timeout",
            "desktop app launch thread did not join after completion",
            ToolOutcome::Unknown,
            false,
            "Observe the current desktop before deciding whether the launch succeeded.",
        )),
    }
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

fn launch_blocking(
    desktop_id: &str,
    cancel: &AtomicBool,
    progress: &ActionProgress,
) -> Result<LaunchResult, RuntimeError> {
    let DesktopEntry { info, app } = installed_entries()
        .find(|entry| entry.info.desktop_id == desktop_id)
        .ok_or_else(|| {
            RuntimeError::new(
                "target_unavailable",
                format!("installed desktop application not found: {desktop_id:?}"),
                ToolOutcome::NotStarted,
                false,
                "Call list_desktop with scope=applications and use an exact returned desktop_id.",
            )
        })?;
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|error| backend_error(format!("cannot open /dev/null for app launch: {error}")))?;
    if cancel.load(Ordering::Acquire) {
        return Err(RuntimeError::new(
            "cancelled",
            "desktop application launch was cancelled before dispatch",
            ToolOutcome::NotStarted,
            true,
            "Retry the launch only if it is still needed.",
        ));
    }
    progress.mark_started();
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
    progress.mark_completed();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn launch_worker_clears_lifecycle_flag_after_join_timeout() {
        let in_progress = Arc::new(AtomicBool::new(true));
        let reset = LaunchReset(Arc::clone(&in_progress));
        let task = LaunchTask {
            cancel: Arc::new(AtomicBool::new(false)),
            join: std::thread::spawn(move || {
                let _reset = reset;
                std::thread::sleep(Duration::from_millis(40));
            }),
        };
        let tasks = Arc::new(LaunchTasks {
            task: std::sync::Mutex::new(Some(task)),
        });

        let error = join_task(&tasks, Arc::clone(&in_progress), Duration::from_millis(1))
            .await
            .unwrap_err();
        assert_eq!(error.code, "backend_timeout");
        assert!(in_progress.load(Ordering::Acquire));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!in_progress.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn cancellation_joins_worker_and_clears_lifecycle_flag() {
        let in_progress = Arc::new(AtomicBool::new(true));
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let reset = LaunchReset(Arc::clone(&in_progress));
        let task = LaunchTask {
            cancel,
            join: std::thread::spawn(move || {
                let _reset = reset;
                while !thread_cancel.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            }),
        };
        let tasks = Arc::new(LaunchTasks {
            task: std::sync::Mutex::new(Some(task)),
        });

        cancel_and_join(Arc::clone(&in_progress), tasks, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(!in_progress.load(Ordering::Acquire));
    }
}
