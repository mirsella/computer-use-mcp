use std::{sync::Arc, time::Duration};

use tokio::time::sleep;

use super::backend::{
    ActionProgress, HeldInput, HeldInputGuard, InputBackend, InputEvent, finish_with_cleanup,
};

const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(80);
const DRAG_STEPS: usize = 16;
const DRAG_STEP_INTERVAL: Duration = Duration::from_millis(8);

pub fn button_code(button: crate::validation::MouseButton) -> u32 {
    match button {
        crate::validation::MouseButton::Left => 272,
        crate::validation::MouseButton::Right => 273,
        crate::validation::MouseButton::Middle => 274,
    }
}

pub async fn move_pointer(
    backend: Arc<dyn InputBackend>,
    x: f64,
    y: f64,
    progress: Arc<ActionProgress>,
) -> Result<(), String> {
    let mut guard = HeldInputGuard::new(Arc::clone(&backend));
    guard.begin().await?;
    progress.mark_started();
    let result = backend.emit(InputEvent::Absolute { x, y }).await;
    if result.is_ok() {
        progress.mark_completed();
    }
    finish_with_cleanup(result, &mut guard, &progress).await
}

pub async fn click(
    backend: Arc<dyn InputBackend>,
    x: f64,
    y: f64,
    button: crate::validation::MouseButton,
    count: usize,
    progress: Arc<ActionProgress>,
) -> Result<(), String> {
    if count == 0 {
        return Err("click count must be positive".into());
    }
    let held = HeldInput::Button(button_code(button));
    let mut guard = HeldInputGuard::new(Arc::clone(&backend));
    guard.begin().await?;
    let result = async {
        progress.mark_started();
        backend.emit(InputEvent::Absolute { x, y }).await?;
        for index in 0..count {
            guard.press(held).await?;
            guard.release(held).await?;
            if index + 1 < count {
                sleep(MULTI_CLICK_INTERVAL).await;
            }
        }
        progress.mark_completed();
        Ok(())
    }
    .await;
    finish_with_cleanup(result, &mut guard, &progress).await
}

pub async fn drag_path(
    backend: Arc<dyn InputBackend>,
    path: Vec<(f64, f64)>,
    progress: Arc<ActionProgress>,
) -> Result<(), String> {
    if path.len() < 2 {
        return Err("drag path must contain at least two points".into());
    }
    let held = HeldInput::Button(272);
    let mut guard = HeldInputGuard::new(Arc::clone(&backend));
    guard.begin().await?;
    let drag = async {
        progress.mark_started();
        let from = path[0];
        backend
            .emit(InputEvent::Absolute {
                x: from.0,
                y: from.1,
            })
            .await?;
        guard.press(held).await?;
        for (segment, points) in path.windows(2).enumerate() {
            let from = points[0];
            let to = points[1];
            for step in 1..=DRAG_STEPS {
                let fraction = step as f64 / DRAG_STEPS as f64;
                backend
                    .emit(InputEvent::Absolute {
                        x: from.0 + (to.0 - from.0) * fraction,
                        y: from.1 + (to.1 - from.1) * fraction,
                    })
                    .await?;
                if segment + 1 != path.len() - 1 || step != DRAG_STEPS {
                    sleep(DRAG_STEP_INTERVAL).await;
                }
            }
        }
        guard.release(held).await?;
        progress.mark_completed();
        Ok(())
    }
    .await;
    finish_with_cleanup(drag, &mut guard, &progress).await
}

pub async fn scroll(
    backend: Arc<dyn InputBackend>,
    x: f64,
    y: f64,
    delta_x: i32,
    delta_y: i32,
    progress: Arc<ActionProgress>,
) -> Result<(), String> {
    if delta_x == 0 && delta_y == 0 {
        return Err("scroll delta must not be zero".into());
    }
    let mut guard = HeldInputGuard::new(Arc::clone(&backend));
    guard.begin().await?;
    let result = async {
        progress.mark_started();
        backend.emit(InputEvent::Absolute { x, y }).await?;
        backend
            .emit(InputEvent::ScrollDiscrete {
                x: delta_x,
                y: delta_y,
            })
            .await?;
        progress.mark_completed();
        Ok(())
    }
    .await;
    finish_with_cleanup(result, &mut guard, &progress).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::backend::{InputEvent, test_support::FakeBackend};

    fn progress() -> Arc<ActionProgress> {
        Arc::new(ActionProgress::default())
    }

    #[tokio::test]
    async fn move_pointer_emits_no_button_event() {
        let fake = FakeBackend::new();
        move_pointer(fake.clone(), 10.0, 20.0, progress())
            .await
            .unwrap();
        assert_eq!(
            *fake.events.lock().unwrap(),
            [InputEvent::Absolute { x: 10.0, y: 20.0 }]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn click_emits_complete_counted_button_sequences() {
        let fake = FakeBackend::new();
        click(
            fake.clone(),
            10.0,
            20.0,
            crate::validation::MouseButton::Right,
            2,
            progress(),
        )
        .await
        .unwrap();
        assert_eq!(
            *fake.events.lock().unwrap(),
            [
                InputEvent::Absolute { x: 10.0, y: 20.0 },
                InputEvent::Button {
                    code: 273,
                    pressed: true,
                },
                InputEvent::Button {
                    code: 273,
                    pressed: false,
                },
                InputEvent::Button {
                    code: 273,
                    pressed: true,
                },
                InputEvent::Button {
                    code: 273,
                    pressed: false,
                },
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn drag_ends_exactly_and_cleans_up_on_error_and_cancellation() {
        let fake = FakeBackend::new();
        drag_path(fake.clone(), vec![(1.0, 2.0), (17.0, 18.0)], progress())
            .await
            .unwrap();
        assert_eq!(
            fake.events.lock().unwrap().last(),
            Some(&InputEvent::Button {
                code: 272,
                pressed: false
            })
        );
        assert_eq!(
            fake.events.lock().unwrap()[17],
            InputEvent::Absolute { x: 17.0, y: 18.0 }
        );

        let failing = FakeBackend::new();
        failing
            .fail_at
            .store(3, std::sync::atomic::Ordering::Release);
        assert!(
            drag_path(failing.clone(), vec![(0.0, 0.0), (2.0, 2.0)], progress(),)
                .await
                .is_err()
        );
        assert!(failing.emergency.lock().unwrap().is_empty());
        assert_eq!(
            failing.events.lock().unwrap().last(),
            Some(&InputEvent::Button {
                code: 272,
                pressed: false
            })
        );

        let cancelled = FakeBackend::new();
        let task = tokio::spawn(drag_path(
            cancelled.clone(),
            vec![(0.0, 0.0), (20.0, 20.0)],
            progress(),
        ));
        sleep(Duration::from_millis(20)).await;
        task.abort();
        let _ = task.await;
        assert_eq!(
            *cancelled.emergency.lock().unwrap(),
            [HeldInput::Button(272)]
        );
    }

    #[tokio::test]
    async fn scroll_moves_then_emits_one_discrete_wheel_event() {
        let fake = FakeBackend::new();
        scroll(fake.clone(), 10.0, 20.0, 0, 240, progress())
            .await
            .unwrap();
        assert_eq!(
            *fake.events.lock().unwrap(),
            [
                InputEvent::Absolute { x: 10.0, y: 20.0 },
                InputEvent::ScrollDiscrete { x: 0, y: 240 },
            ]
        );
        assert!(scroll(fake, 10.0, 20.0, 0, 0, progress()).await.is_err());
    }
}
