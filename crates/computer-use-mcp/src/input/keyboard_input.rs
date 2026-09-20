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
    MAX_KEYBOARD_TRANSACTION_TEXT, MAX_TEXT_LIMIT, MouseButton,
};

const FOCUS_SETTLE_DELAY: Duration = Duration::from_millis(50);

type ResolvedStroke = (Vec<KeyboardKey>, KeyboardKey);
type ResolvedAction = Vec<ResolvedStroke>;
type ResolvedTransaction = Vec<ResolvedAction>;

struct ResolvedPaste {
    keys: Vec<ResolvedKey>,
    chunk_ends: Vec<usize>,
}

impl ResolvedPaste {
    fn chunks(&self) -> impl Iterator<Item = &[ResolvedKey]> {
        let mut start = 0;
        self.chunk_ends.iter().map(move |&end| {
            let chunk = &self.keys[start..end];
            start = end;
            chunk
        })
    }
}

pub fn preflight_transaction(
    focus: Option<KeyboardPoint>,
    events: &[KeyboardEvent],
) -> Result<(), String> {
    validate_transaction_shape(events)?;
    if let Some(focus) = focus {
        validate_focus(focus)?;
    }
    Ok(())
}

pub async fn perform_transaction(
    backend: Arc<ReisInputBackend>,
    focus: Option<KeyboardPoint>,
    events: Vec<KeyboardEvent>,
    progress: Arc<ActionProgress>,
) -> Result<(), String> {
    if let Some(focus) = focus {
        validate_focus(focus)?;
    }
    let resolved = resolve_transaction(&backend, &events)?;
    match focus {
        Some(focus) => tap_sequence(backend, focus, resolved, progress).await,
        None => type_into_focus(backend, resolved, progress).await,
    }
}

/// Split paste text into bounded typing chunks. The split is on Unicode scalar
/// value boundaries so no chunk exceeds the single-transaction text limit.
pub fn paste_chunks(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    paste_chunk_slices(text).map(str::to_owned).collect()
}

struct PasteChunkSlices<'a> {
    text: &'a str,
    next_byte: usize,
}

impl<'a> Iterator for PasteChunkSlices<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_byte == self.text.len() {
            return None;
        }
        let start = self.next_byte;
        let mut end = start;
        for (count, (offset, scalar)) in self.text[start..].char_indices().enumerate() {
            if count == MAX_KEYBOARD_TRANSACTION_TEXT {
                break;
            }
            end = start + offset + scalar.len_utf8();
        }
        self.next_byte = end;
        Some(&self.text[start..end])
    }
}

fn paste_chunk_slices(text: &str) -> PasteChunkSlices<'_> {
    PasteChunkSlices { text, next_byte: 0 }
}

fn validate_paste_shape(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("paste text must not be empty".into());
    }
    if text.chars().count() > MAX_TEXT_LIMIT {
        return Err(format!(
            "paste text must contain at most {MAX_TEXT_LIMIT} Unicode scalar values"
        ));
    }
    if text.contains('\0') {
        return Err("paste text must not contain NUL".into());
    }
    Ok(())
}

pub fn preflight_paste(focus: Option<KeyboardPoint>, text: &str) -> Result<usize, String> {
    validate_paste_shape(text)?;
    if let Some(focus) = focus {
        validate_focus(focus)?;
    }
    Ok(paste_chunk_slices(text).count())
}

/// Paste long text without touching any clipboard, optionally focus-clicking a
/// point first. Each chunk runs through the same cleanup-guarded transaction
/// path as a single `type` event, so held keys are released even when a later
/// chunk fails. Never logs the text itself.
pub async fn perform_paste(
    backend: Arc<ReisInputBackend>,
    focus: Option<KeyboardPoint>,
    text: String,
    progress: Arc<ActionProgress>,
) -> Result<usize, String> {
    validate_paste_shape(&text)?;
    if let Some(focus) = focus {
        validate_focus(focus)?;
    }
    let resolved = resolve_paste(&backend, &text)?;
    stream_paste(backend, focus, resolved, progress).await
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
        KeyboardEvent::Type(text) => Ok(resolved_text_action(&resolve_text_keys(backend, text)?)),
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

fn resolve_text_keys(backend: &ReisInputBackend, text: &str) -> Result<Vec<ResolvedKey>, String> {
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
    let resolved = backend.resolve_keysyms(&keysyms)?;
    if resolved.len() > MAX_KEYBOARD_EXPANDED_ACTIONS {
        return Err(format!(
            "keyboard expanded action count exceeds {MAX_KEYBOARD_EXPANDED_ACTIONS}"
        ));
    }
    if resolved.len() != keysyms.len() {
        return Err("EIS returned an incomplete keyboard resolution".into());
    }
    Ok(resolved)
}

fn resolved_text_action(resolved: &[ResolvedKey]) -> ResolvedAction {
    resolved
        .iter()
        .map(|key| {
            let modifiers = key
                .modifiers
                .into_iter()
                .flatten()
                .map(|keycode| keyboard_key(key, keycode))
                .collect();
            (modifiers, keyboard_key(key, key.keycode))
        })
        .collect()
}

fn resolve_paste(backend: &ReisInputBackend, text: &str) -> Result<ResolvedPaste, String> {
    let mut keys = Vec::new();
    let mut chunk_ends = Vec::new();
    for chunk in paste_chunk_slices(text) {
        keys.extend(resolve_text_keys(backend, chunk)?);
        chunk_ends.push(keys.len());
    }
    Ok(ResolvedPaste { keys, chunk_ends })
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
        emit_keys(&backend, &mut guard, actions).await?;
        progress.mark_completed();
        Ok(())
    }
    .await;
    super::backend::finish_with_cleanup(result, &mut guard, &progress).await
}

async fn stream_paste<B>(
    backend: Arc<B>,
    focus: Option<KeyboardPoint>,
    resolved: ResolvedPaste,
    progress: Arc<ActionProgress>,
) -> Result<usize, String>
where
    B: InputBackend,
{
    let backend: Arc<dyn InputBackend> = backend;
    let mut guard = HeldInputGuard::new(Arc::clone(&backend));
    guard.begin().await?;
    let result = async {
        progress.mark_started();
        if let Some(focus) = focus {
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
        }

        let chunk_count = resolved.chunk_ends.len();
        for (index, chunk) in resolved.chunks().enumerate() {
            emit_keys(&backend, &mut guard, vec![resolved_text_action(chunk)])
                .await
                .map_err(|error| format!("paste chunk {} failed: {error}", index + 1))?;
            if index + 1 < chunk_count {
                backend.sync_barrier().await.map_err(|error| {
                    format!("paste chunk {} synchronization failed: {error}", index + 1)
                })?;
            }
        }
        progress.mark_completed();
        Ok(chunk_count)
    }
    .await;
    super::backend::finish_with_cleanup(result, &mut guard, &progress).await
}

/// Type resolved key actions into the already-focused element: no pointer
/// movement, no click, no focus barrier. Held keys are released through the
/// same cleanup guard as [`tap_sequence`], so interrupted typing still
/// restores modifiers.
async fn type_into_focus<B>(
    backend: Arc<B>,
    actions: ResolvedTransaction,
    progress: Arc<ActionProgress>,
) -> Result<(), String>
where
    B: InputBackend,
{
    let backend: Arc<dyn InputBackend> = backend;
    let mut guard = HeldInputGuard::new(Arc::clone(&backend));
    guard.begin().await?;
    let result = async {
        progress.mark_started();
        emit_keys(&backend, &mut guard, actions).await?;
        progress.mark_completed();
        Ok(())
    }
    .await;
    super::backend::finish_with_cleanup(result, &mut guard, &progress).await
}

async fn emit_keys(
    backend: &Arc<dyn InputBackend>,
    guard: &mut HeldInputGuard,
    actions: ResolvedTransaction,
) -> Result<(), String> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::input::backend::test_support::{FakeBackend, TraceEvent};
    use crate::runtime::DispatchStage;

    fn progress() -> Arc<ActionProgress> {
        Arc::new(ActionProgress::default())
    }

    #[test]
    fn paste_chunks_split_on_scalar_boundaries_and_rejoin() {
        assert_eq!(paste_chunks("hello"), vec!["hello".to_owned()]);
        let long = "a".repeat(MAX_KEYBOARD_TRANSACTION_TEXT * 2 + 7);
        let chunks = paste_chunks(&long);
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= MAX_KEYBOARD_TRANSACTION_TEXT)
        );
        assert_eq!(chunks.concat(), long);
        // Multi-byte scalars straddling a chunk boundary stay intact.
        let wide = "é".repeat(MAX_KEYBOARD_TRANSACTION_TEXT + 1);
        let chunks = paste_chunks(&wide);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), wide);
        for chunk in &chunks {
            assert!(chunk.is_char_boundary(chunk.len()));
        }
    }

    fn resolved_paste(key_count: usize) -> ResolvedPaste {
        let keys = (0..key_count)
            .map(|index| ResolvedKey {
                device_id: 7,
                resume_generation: 2,
                keycode: 30 + (index as u32 % 4),
                modifiers: [None; 4],
            })
            .collect();
        let mut chunk_ends = Vec::new();
        let mut end = 0;
        while end < key_count {
            end = (end + MAX_KEYBOARD_TRANSACTION_TEXT).min(key_count);
            chunk_ends.push(end);
        }
        ResolvedPaste { keys, chunk_ends }
    }

    #[tokio::test(start_paused = true)]
    async fn long_paste_focuses_once_and_keeps_chunk_order() {
        let backend = FakeBackend::new();
        let progress = progress();
        stream_paste(
            Arc::clone(&backend),
            Some(KeyboardPoint { x: 10.0, y: 20.0 }),
            resolved_paste(MAX_KEYBOARD_TRANSACTION_TEXT + 1),
            Arc::clone(&progress),
        )
        .await
        .unwrap();

        let events = backend.events.lock().unwrap().clone();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, InputEvent::Absolute { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, InputEvent::Button { .. }))
                .count(),
            2
        );
        let first_key = events
            .iter()
            .position(|event| matches!(event, InputEvent::Keycode { pressed: true, .. }))
            .unwrap();
        assert!(matches!(
            events[first_key - 1],
            InputEvent::Button { pressed: false, .. }
        ));
        let key_presses = events
            .iter()
            .filter_map(|event| match event {
                InputEvent::Keycode { key, pressed: true } => Some(key.keycode),
                _ => None,
            })
            .collect::<Vec<_>>();
        let expected_keycodes = (0..MAX_KEYBOARD_TRANSACTION_TEXT + 1)
            .map(|index| 30 + (index as u32 % 4))
            .collect::<Vec<_>>();
        assert_eq!(key_presses, expected_keycodes);
        // One barrier flushes the focus click and one separates the two
        // bounded chunks; neither starts a new input lifecycle.
        assert_eq!(backend.sync_calls.load(Ordering::Acquire), 2);
        assert_eq!(progress.snapshot().dispatch_stage, DispatchStage::Completed);
    }

    #[tokio::test(start_paused = true)]
    async fn focused_long_paste_streams_without_pointer_focus() {
        let backend = FakeBackend::new();
        let progress = progress();
        stream_paste(
            Arc::clone(&backend),
            None,
            resolved_paste(MAX_KEYBOARD_TRANSACTION_TEXT + 1),
            Arc::clone(&progress),
        )
        .await
        .unwrap();

        assert!(
            backend
                .events
                .lock()
                .unwrap()
                .iter()
                .all(|event| matches!(event, InputEvent::Keycode { .. }))
        );
        assert_eq!(backend.sync_calls.load(Ordering::Acquire), 1);
        assert_eq!(progress.snapshot().dispatch_stage, DispatchStage::Completed);
    }

    #[tokio::test(start_paused = true)]
    async fn later_paste_chunk_failure_does_not_complete_progress() {
        let backend = FakeBackend::new();
        // Three focus events and two events for each key in the first chunk.
        backend
            .fail_at
            .store(3 + 2 * MAX_KEYBOARD_TRANSACTION_TEXT, Ordering::Release);
        let progress = progress();
        let error = stream_paste(
            Arc::clone(&backend),
            Some(KeyboardPoint { x: 10.0, y: 20.0 }),
            resolved_paste(MAX_KEYBOARD_TRANSACTION_TEXT + 1),
            Arc::clone(&progress),
        )
        .await
        .unwrap_err();

        assert!(error.contains("paste chunk 2 failed"));
        assert_eq!(progress.snapshot().dispatch_stage, DispatchStage::Started);
        assert_eq!(backend.cleanup_calls.load(Ordering::Acquire), 1);
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

    #[tokio::test(start_paused = true)]
    async fn focused_typing_emits_keys_without_pointer_or_click() {
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
        type_into_focus(
            Arc::clone(&backend),
            vec![vec![(vec![modifier], key)]],
            progress(),
        )
        .await
        .unwrap();

        // No pointer movement, no click, no focus barrier: only keystrokes.
        assert!(
            backend
                .events
                .lock()
                .unwrap()
                .iter()
                .all(|event| matches!(event, InputEvent::Keycode { .. })),
            "focused typing must not emit pointer events"
        );
        assert_eq!(
            *backend.events.lock().unwrap(),
            vec![
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
        assert_eq!(backend.sync_calls.load(Ordering::Acquire), 0);
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
