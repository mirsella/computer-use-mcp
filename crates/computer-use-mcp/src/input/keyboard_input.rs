use std::{sync::Arc, time::Duration};

use tokio::time::sleep;

use super::{
    backend::{ActionProgress, HeldInput, HeldInputGuard, InputBackend, InputEvent, KeyboardKey},
    eis::{ReisInputBackend, ResolvedKey},
    keyboard::{KeyChord, parse_chord, unicode_keysym},
    pointer::button_code,
};
use crate::validation::{
    KeyboardEvent, KeyboardPoint, MAX_KEYBOARD_EVENTS, MAX_KEYBOARD_EXPANDED_ACTIONS,
    MAX_KEYBOARD_TRANSACTION_TEXT, MouseButton,
};

const FOCUS_SETTLE_DELAY: Duration = Duration::from_millis(50);

type ResolvedStroke = (Vec<KeyboardKey>, KeyboardKey);
type ResolvedAction = Vec<ResolvedStroke>;
type ResolvedTransaction = Vec<ResolvedAction>;

pub fn preflight_transaction(
    backend: &ReisInputBackend,
    focus: KeyboardPoint,
    events: &[KeyboardEvent],
) -> Result<(), String> {
    validate_transaction_shape(events)?;
    validate_focus(focus)?;
    resolve_transaction(backend, events).map(drop)?;
    Ok(())
}

pub async fn perform_transaction(
    backend: Arc<ReisInputBackend>,
    focus: KeyboardPoint,
    events: Vec<KeyboardEvent>,
    progress: Arc<ActionProgress>,
) -> Result<(), String> {
    validate_focus(focus)?;
    let resolved = resolve_transaction(&backend, &events)?;
    tap_sequence(backend, focus, resolved, progress).await
}

fn validate_focus(focus: KeyboardPoint) -> Result<(), String> {
    if !focus.x.is_finite() || !focus.y.is_finite() {
        return Err("keyboard focus point must be finite".into());
    }
    Ok(())
}

fn validate_transaction_shape(events: &[KeyboardEvent]) -> Result<(), String> {
    if events.is_empty() || events.len() > MAX_KEYBOARD_EVENTS {
        return Err(format!(
            "keyboard transactions must contain 1 through {MAX_KEYBOARD_EVENTS} events"
        ));
    }
    let has_type = events
        .iter()
        .any(|event| matches!(event, KeyboardEvent::Type(_)));
    if has_type {
        if events.len() != 1 || !matches!(events, [KeyboardEvent::Type(_)]) {
            return Err(
                "keyboard events must be either press-only or exactly one type event; mixed press/type transactions are rejected".into(),
            );
        }
    } else if !events
        .iter()
        .all(|event| matches!(event, KeyboardEvent::Press(_)))
    {
        return Err("keyboard event type is unsupported".into());
    }
    Ok(())
}

fn resolve_transaction(
    backend: &ReisInputBackend,
    events: &[KeyboardEvent],
) -> Result<ResolvedTransaction, String> {
    validate_transaction_shape(events)?;
    events
        .iter()
        .map(|action| resolve_action(backend, action))
        .collect()
}

fn resolve_action(
    backend: &ReisInputBackend,
    action: &KeyboardEvent,
) -> Result<ResolvedAction, String> {
    match action {
        KeyboardEvent::Press(chord) => resolve_chord(backend, &parse_chord(chord)?),
        KeyboardEvent::Type(text) => {
            if text.is_empty() {
                return Err("keyboard type event text must not be empty".into());
            }
            if text.chars().count() > MAX_KEYBOARD_TRANSACTION_TEXT {
                return Err(format!(
                    "keyboard type event text must contain at most {MAX_KEYBOARD_TRANSACTION_TEXT} Unicode scalar values"
                ));
            }
            let keysyms = text
                .chars()
                .map(unicode_keysym)
                .collect::<Result<Vec<_>, _>>()?;
            let resolved = resolve_text(backend, &keysyms)?;
            if resolved.len() > MAX_KEYBOARD_EXPANDED_ACTIONS {
                return Err(format!(
                    "keyboard expanded action count exceeds {MAX_KEYBOARD_EXPANDED_ACTIONS}"
                ));
            }
            Ok(resolved)
        }
    }
}

fn resolve_chord(backend: &ReisInputBackend, chord: &KeyChord) -> Result<ResolvedAction, String> {
    let keysyms = chord
        .modifiers
        .iter()
        .copied()
        .chain(std::iter::once(chord.key))
        .collect::<Vec<_>>();
    let mut resolved = backend.resolve_keysyms(&keysyms)?;
    let key = resolved.pop().ok_or("key chord is empty")?;
    let mut modifiers = Vec::new();
    for modifier in resolved {
        modifiers.extend(
            modifier
                .modifiers
                .into_iter()
                .flatten()
                .map(|keycode| keyboard_key(&modifier, keycode)),
        );
        modifiers.push(keyboard_key(&modifier, modifier.keycode));
    }
    modifiers.extend(
        key.modifiers
            .into_iter()
            .flatten()
            .map(|keycode| keyboard_key(&key, keycode)),
    );
    modifiers.sort_unstable_by_key(|key| key.keycode);
    modifiers.dedup();
    Ok(vec![(modifiers, keyboard_key(&key, key.keycode))])
}

fn resolve_text(backend: &ReisInputBackend, keysyms: &[u32]) -> Result<ResolvedAction, String> {
    backend
        .resolve_keysyms(keysyms)?
        .into_iter()
        .map(|key| {
            let modifiers = key
                .modifiers
                .into_iter()
                .flatten()
                .map(|keycode| keyboard_key(&key, keycode))
                .collect();
            Ok((modifiers, keyboard_key(&key, key.keycode)))
        })
        .collect()
}

fn keyboard_key(resolved: &ResolvedKey, keycode: u32) -> KeyboardKey {
    KeyboardKey {
        device_id: resolved.device_id,
        resume_generation: resolved.resume_generation,
        keycode,
    }
}

async fn tap_sequence<B>(
    backend: Arc<B>,
    focus: KeyboardPoint,
    actions: ResolvedTransaction,
    progress: Arc<ActionProgress>,
) -> Result<(), String>
where
    B: InputBackend,
{
    validate_focus(focus)?;
    let backend: Arc<dyn InputBackend> = backend;
    let mut guard = HeldInputGuard::new(Arc::clone(&backend));
    guard.begin().await?;
    let result = async {
        progress.mark_started();
        backend
            .emit(InputEvent::Absolute {
                x: focus.x,
                y: focus.y,
            })
            .await?;
        let button = HeldInput::Button(button_code(MouseButton::Left));
        guard.press(button).await?;
        guard.release(button).await?;
        backend.sync_barrier().await.map_err(|error| {
            format!("focus click was dispatched but synchronization failed: {error}")
        })?;
        sleep(FOCUS_SETTLE_DELAY).await;
        let action_count = actions.len();
        for (index, action) in actions.into_iter().enumerate() {
            for (modifiers, keycode) in action {
                for &modifier in &modifiers {
                    guard.press(HeldInput::Keycode(modifier)).await?;
                }
                let key = HeldInput::Keycode(keycode);
                guard.press(key).await?;
                guard.release(key).await?;
                for &modifier in modifiers.iter().rev() {
                    guard.release(HeldInput::Keycode(modifier)).await?;
                }
            }
            if index + 1 < action_count {
                backend.sync_barrier().await.map_err(|error| {
                    format!("keyboard phase dispatched but synchronization failed: {error}")
                })?;
                sleep(FOCUS_SETTLE_DELAY).await;
            }
        }
        progress.mark_completed();
        Ok(())
    }
    .await;
    super::backend::finish_with_cleanup(result, &mut guard, &progress).await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::input::backend::test_support::{FakeBackend, TraceEvent};

    fn progress() -> Arc<ActionProgress> {
        Arc::new(ActionProgress::default())
    }

    #[tokio::test(start_paused = true)]
    async fn keyboard_sequence_focuses_the_visible_point_before_typing() {
        let backend = FakeBackend::new();

        let modifier = KeyboardKey {
            device_id: 7,
            resume_generation: 2,
            keycode: 42,
        };
        let key = KeyboardKey {
            keycode: 30,
            ..modifier
        };
        tap_sequence(
            Arc::clone(&backend),
            KeyboardPoint { x: 125.5, y: 80.25 },
            vec![vec![(vec![modifier], key)]],
            progress(),
        )
        .await
        .unwrap();
        let trace = backend.trace.lock().unwrap().clone();
        assert!(matches!(
            trace.as_slice(),
            [
                TraceEvent::Emit(InputEvent::Absolute { .. }),
                TraceEvent::Emit(InputEvent::Button { pressed: true, .. }),
                TraceEvent::Emit(InputEvent::Button { pressed: false, .. }),
                TraceEvent::SyncBarrier,
                TraceEvent::Emit(InputEvent::Keycode { pressed: true, .. }),
                ..
            ]
        ));

        assert_eq!(
            *backend.events.lock().unwrap(),
            vec![
                InputEvent::Absolute { x: 125.5, y: 80.25 },
                InputEvent::Button {
                    code: 272,
                    pressed: true,
                },
                InputEvent::Button {
                    code: 272,
                    pressed: false,
                },
                InputEvent::Keycode {
                    key: modifier,
                    pressed: true,
                },
                InputEvent::Keycode { key, pressed: true },
                InputEvent::Keycode {
                    key,
                    pressed: false,
                },
                InputEvent::Keycode {
                    key: modifier,
                    pressed: false,
                },
            ]
        );
        assert_eq!(backend.sync_calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn keyboard_sequence_always_focuses_before_typing() {
        let backend = FakeBackend::new();
        let key = KeyboardKey {
            device_id: 7,
            resume_generation: 2,
            keycode: 30,
        };

        tap_sequence(
            Arc::clone(&backend),
            KeyboardPoint { x: 10.0, y: 20.0 },
            vec![vec![(Vec::new(), key)]],
            progress(),
        )
        .await
        .unwrap();

        assert_eq!(
            *backend.events.lock().unwrap(),
            vec![
                InputEvent::Absolute { x: 10.0, y: 20.0 },
                InputEvent::Button {
                    code: 272,
                    pressed: true,
                },
                InputEvent::Button {
                    code: 272,
                    pressed: false,
                },
                InputEvent::Keycode { key, pressed: true },
                InputEvent::Keycode {
                    key,
                    pressed: false,
                },
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn focus_barrier_failure_sends_no_key_and_runs_cleanup() {
        let backend = FakeBackend::new();
        backend.fail_sync.store(true, Ordering::Release);
        let key = KeyboardKey {
            device_id: 7,
            resume_generation: 2,
            keycode: 30,
        };

        let error = tap_sequence(
            Arc::clone(&backend),
            KeyboardPoint { x: 10.0, y: 20.0 },
            vec![vec![(Vec::new(), key)]],
            progress(),
        )
        .await
        .unwrap_err();

        assert!(error.contains("focus click was dispatched"));
        assert!(
            backend
                .events
                .lock()
                .unwrap()
                .iter()
                .all(|event| !matches!(event, InputEvent::Keycode { .. }))
        );
        assert_eq!(backend.sync_calls.load(Ordering::Acquire), 1);
        assert_eq!(backend.cleanup_calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn press_phases_sync_between_high_level_events() {
        let backend = FakeBackend::new();
        let first = KeyboardKey {
            device_id: 7,
            resume_generation: 2,
            keycode: 30,
        };
        let second = KeyboardKey {
            keycode: 31,
            ..first
        };

        tap_sequence(
            Arc::clone(&backend),
            KeyboardPoint { x: 10.0, y: 20.0 },
            vec![vec![(Vec::new(), first)], vec![(Vec::new(), second)]],
            progress(),
        )
        .await
        .unwrap();

        let trace = backend.trace.lock().unwrap().clone();
        let barriers = trace
            .iter()
            .enumerate()
            .filter_map(|(index, event)| matches!(event, TraceEvent::SyncBarrier).then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(barriers.len(), 2);
        assert!(barriers[0] < trace
            .iter()
            .position(|event| matches!(event, TraceEvent::Emit(InputEvent::Keycode { key, pressed: true }) if *key == first))
            .unwrap());
        assert!(barriers[1] < trace
            .iter()
            .position(|event| matches!(event, TraceEvent::Emit(InputEvent::Keycode { key, pressed: true }) if *key == second))
            .unwrap());
    }
}
