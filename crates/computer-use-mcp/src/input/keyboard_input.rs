use std::{sync::Arc, time::Duration};

use tokio::time::sleep;

use super::{
    backend::{HeldInput, HeldInputGuard, InputBackend, InputEvent, KeyboardKey},
    eis::{ReisInputBackend, ResolvedKey},
    keyboard::{KeyChord, parse_chord, unicode_keysym},
    pointer::button_code,
};
use crate::validation::{KeyboardAction, MouseButton};

const FOCUS_SETTLE_DELAY: Duration = Duration::from_millis(50);

pub fn preflight(backend: &ReisInputBackend, action: &KeyboardAction) -> Result<(), String> {
    resolve_action(backend, action).map(drop)
}

pub async fn perform(
    backend: Arc<ReisInputBackend>,
    focus: Option<(f64, f64)>,
    action: KeyboardAction,
) -> Result<(), String> {
    if focus.is_some() {
        resolve_action(&backend, &action)?;
    }
    let resolver = Arc::clone(&backend);
    tap_sequence(backend, focus, move || resolve_action(&resolver, &action)).await
}

fn resolve_action(
    backend: &ReisInputBackend,
    action: &KeyboardAction,
) -> Result<Vec<(Vec<KeyboardKey>, KeyboardKey)>, String> {
    match action {
        KeyboardAction::Press(chord) => resolve_chord(backend, &parse_chord(chord)?),
        KeyboardAction::Type(text) => {
            let keysyms = text
                .chars()
                .map(unicode_keysym)
                .collect::<Result<Vec<_>, _>>()?;
            resolve_text(backend, &keysyms)
        }
    }
}

fn resolve_chord(
    backend: &ReisInputBackend,
    chord: &KeyChord,
) -> Result<Vec<(Vec<KeyboardKey>, KeyboardKey)>, String> {
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

fn resolve_text(
    backend: &ReisInputBackend,
    keysyms: &[u32],
) -> Result<Vec<(Vec<KeyboardKey>, KeyboardKey)>, String> {
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

async fn tap_sequence<B, F>(
    backend: Arc<B>,
    focus: Option<(f64, f64)>,
    resolve: F,
) -> Result<(), String>
where
    B: InputBackend,
    F: FnOnce() -> Result<Vec<(Vec<KeyboardKey>, KeyboardKey)>, String>,
{
    let backend: Arc<dyn InputBackend> = backend;
    let mut guard = HeldInputGuard::new(Arc::clone(&backend));
    guard.begin().await?;
    let result = async {
        if let Some((x, y)) = focus {
            backend.emit(InputEvent::Absolute { x, y }).await?;
            let button = HeldInput::Button(button_code(MouseButton::Left));
            guard.press(button).await?;
            guard.release(button).await?;
            sleep(FOCUS_SETTLE_DELAY).await;
        }
        let keys = resolve()?;
        for (modifiers, keycode) in keys {
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
        Ok(())
    }
    .await;
    super::backend::finish_with_cleanup(result, &mut guard).await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::input::backend::test_support::FakeBackend;

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
        let resolved_after_focus = Arc::new(AtomicBool::new(false));
        let observed_backend = Arc::clone(&backend);
        let observed_resolution = Arc::clone(&resolved_after_focus);
        tap_sequence(Arc::clone(&backend), Some((125.5, 80.25)), move || {
            observed_resolution.store(
                observed_backend.events.lock().unwrap().as_slice()
                    == [
                        InputEvent::Absolute { x: 125.5, y: 80.25 },
                        InputEvent::Button {
                            code: 272,
                            pressed: true,
                        },
                        InputEvent::Button {
                            code: 272,
                            pressed: false,
                        },
                    ],
                Ordering::Release,
            );
            Ok(vec![(vec![modifier], key)])
        })
        .await
        .unwrap();
        assert!(resolved_after_focus.load(Ordering::Acquire));

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
    }

    #[tokio::test]
    async fn keyboard_sequence_can_type_without_pointer_focus() {
        let backend = FakeBackend::new();
        let key = KeyboardKey {
            device_id: 7,
            resume_generation: 2,
            keycode: 30,
        };

        tap_sequence(Arc::clone(&backend), None, move || {
            Ok(vec![(Vec::new(), key)])
        })
        .await
        .unwrap();

        assert_eq!(
            *backend.events.lock().unwrap(),
            vec![
                InputEvent::Keycode { key, pressed: true },
                InputEvent::Keycode {
                    key,
                    pressed: false,
                },
            ]
        );
    }
}
