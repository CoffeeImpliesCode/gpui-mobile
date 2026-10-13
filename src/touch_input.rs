//! Adds two-contact pinch recognition ahead of GPUI's single-contact recognizer.
//!
//! One contact stays raw, so elements can claim a free two-axis `TouchDragEvent`.
//! A second contact cancels that raw stream before starting a pinch. Remaining
//! contacts are drained after the pinch ends: they cannot produce a stray tap,
//! resume a stale drag, or start scroll momentum.
//!
//! Like GPUI's Linux backends, `PinchEvent::delta` is the difference between
//! successive absolute scales (span / starting span), not a multiplicative
//! ratio. The focal position is the current midpoint in logical window pixels.

use std::collections::HashMap;

use gpui::{point, PinchEvent, Pixels, PlatformInput, Point, TouchEvent, TouchId, TouchPhase};

use crate::fling_guard::FlingGuard;

pub(crate) struct TouchInput {
    contacts: HashMap<TouchId, Point<Pixels>>,
    single: Option<TouchId>,
    pinch: Option<Pinch>,
    fling_guard: FlingGuard,
}

struct Pinch {
    first: TouchId,
    second: TouchId,
    initial_distance: f64,
    scale: f32,
}

impl TouchInput {
    pub(crate) fn new() -> Self {
        Self {
            contacts: HashMap::new(),
            single: None,
            pinch: None,
            fling_guard: FlingGuard::new(),
        }
    }

    pub(crate) fn relay(
        &mut self,
        event: TouchEvent,
        next_id: impl FnOnce() -> TouchId,
        mut emit: impl FnMut(PlatformInput),
    ) {
        if event.phase == TouchPhase::Started {
            self.start(event, next_id, &mut emit);
            return;
        }
        let Some(position) = self.contacts.get_mut(&event.id) else {
            return;
        };
        let changed = *position != event.position;
        *position = event.position;
        let id = event.id;
        let phase = event.phase;
        if self.single == Some(event.id) {
            self.fling_guard
                .relay(event, next_id, |touch| emit(PlatformInput::Touch(touch)));
        } else if self
            .pinch
            .as_ref()
            .is_some_and(|pinch| pinch.first == id || pinch.second == id)
        {
            self.update_pinch(phase, changed, &mut emit);
        }
        if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.contacts.remove(&id);
            if self.single == Some(id) {
                self.single = None;
            }
        }
    }

    fn start(
        &mut self,
        event: TouchEvent,
        next_id: impl FnOnce() -> TouchId,
        emit: &mut impl FnMut(PlatformInput),
    ) {
        if self.contacts.contains_key(&event.id) {
            return;
        }
        self.contacts.insert(event.id, event.position);
        if self.contacts.len() == 1 {
            self.single = Some(event.id);
            self.fling_guard
                .relay(event, next_id, |touch| emit(PlatformInput::Touch(touch)));
            return;
        }
        let Some(first) = self.single.take() else {
            return;
        };
        let Some(first_position) = self.contacts.get(&first).copied() else {
            return;
        };
        self.fling_guard.relay(
            TouchEvent {
                id: first,
                phase: TouchPhase::Cancelled,
                position: first_position,
                predicted_position: None,
                force: None,
            },
            next_id,
            |touch| emit(PlatformInput::Touch(touch)),
        );
        let pinch = Pinch {
            first,
            second: event.id,
            initial_distance: (event.position - first_position).magnitude(),
            scale: 1.,
        };
        if let Some((position, _)) = pinch.geometry(&self.contacts) {
            emit(pinch_event(position, 0., TouchPhase::Started));
        }
        self.pinch = Some(pinch);
    }

    fn update_pinch(
        &mut self,
        phase: TouchPhase,
        changed: bool,
        emit: &mut impl FnMut(PlatformInput),
    ) {
        let Some(pinch) = self.pinch.as_mut() else {
            return;
        };
        let Some((position, distance)) = pinch.geometry(&self.contacts) else {
            return;
        };
        if phase == TouchPhase::Moved || (phase == TouchPhase::Ended && changed) {
            if pinch.initial_distance == 0. {
                pinch.initial_distance = distance;
            }
            let scale = if pinch.initial_distance > 0. {
                (distance / pinch.initial_distance) as f32
            } else {
                1.
            };
            emit(pinch_event(position, scale - pinch.scale, TouchPhase::Moved));
            pinch.scale = scale;
        }
        if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            emit(pinch_event(position, 0., phase));
            self.pinch = None;
        }
    }
}

impl Pinch {
    fn geometry(
        &self,
        contacts: &HashMap<TouchId, Point<Pixels>>,
    ) -> Option<(Point<Pixels>, f64)> {
        let first = contacts.get(&self.first)?;
        let second = contacts.get(&self.second)?;
        let position = point((first.x + second.x) / 2., (first.y + second.y) / 2.);
        Some((position, (*second - *first).magnitude()))
    }
}

fn pinch_event(position: Point<Pixels>, delta: f32, phase: TouchPhase) -> PlatformInput {
    PlatformInput::Pinch(PinchEvent {
        position,
        delta,
        modifiers: gpui::Modifiers::default(),
        phase,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px};

    fn touch(id: u64, phase: TouchPhase, x: f32, y: f32) -> TouchEvent {
        TouchEvent {
            id: TouchId(id),
            phase,
            position: point(px(x), px(y)),
            predicted_position: None,
            force: None,
        }
    }

    fn relay(input: &mut TouchInput, event: TouchEvent) -> Vec<PlatformInput> {
        let mut events = Vec::new();
        input.relay(event, || TouchId(100), |event| events.push(event));
        events
    }

    fn start_pair(input: &mut TouchInput) {
        relay(input, touch(1, TouchPhase::Started, 0., 0.));
        relay(input, touch(2, TouchPhase::Started, 10., 0.));
    }

    fn pinch(events: &[PlatformInput]) -> &PinchEvent {
        let [PlatformInput::Pinch(event)] = events else {
            panic!("expected one pinch event, got {events:?}");
        };
        event
    }

    #[test]
    fn single_contact_preserves_raw_two_axis_positions_and_release() {
        let mut input = TouchInput::new();
        for phase in [TouchPhase::Started, TouchPhase::Moved, TouchPhase::Ended] {
            let events = relay(&mut input, touch(1, phase, 25., 40.));
            let [PlatformInput::Touch(event)] = events.as_slice() else {
                panic!("expected raw single touch");
            };
            assert_eq!(event.id, TouchId(1));
            assert_eq!(event.phase, phase);
            assert_eq!(event.position, point(px(25.), px(40.)));
        }
    }

    #[test]
    fn second_contact_cancels_the_first_before_starting_pinch() {
        let mut input = TouchInput::new();
        relay(&mut input, touch(1, TouchPhase::Started, 0., 0.));
        relay(&mut input, touch(1, TouchPhase::Moved, 4., 6.));
        let events = relay(&mut input, touch(2, TouchPhase::Started, 10., 12.));
        let [PlatformInput::Touch(cancelled), PlatformInput::Pinch(started)] =
            events.as_slice()
        else {
            panic!("expected cancellation before pinch");
        };
        assert_eq!(cancelled.id, TouchId(1));
        assert_eq!(cancelled.phase, TouchPhase::Cancelled);
        assert_eq!(cancelled.position, point(px(4.), px(6.)));
        assert_eq!(started.phase, TouchPhase::Started);
        assert_eq!(started.position, point(px(7.), px(9.)));
        assert_eq!(started.delta, 0.);
        assert_eq!(started.modifiers, gpui::Modifiers::default());
    }

    #[test]
    fn pinch_deltas_are_differences_of_absolute_scale_not_ratios() {
        let mut input = TouchInput::new();
        start_pair(&mut input);
        let first = relay(&mut input, touch(1, TouchPhase::Moved, -10., 0.));
        assert_eq!(pinch(&first).phase, TouchPhase::Moved);
        assert_eq!(pinch(&first).delta, 1.);
        assert_eq!(pinch(&first).position, point(px(0.), px(0.)));
        let second = relay(&mut input, touch(2, TouchPhase::Moved, 20., 0.));
        assert_eq!(pinch(&second).delta, 1.);
        assert_eq!(pinch(&second).position, point(px(5.), px(0.)));
        let contraction = relay(&mut input, touch(1, TouchPhase::Moved, 0., 0.));
        assert_eq!(pinch(&contraction).delta, -1.);
        assert_eq!(pinch(&contraction).position, point(px(10.), px(0.)));
    }

    #[test]
    fn pinch_uses_euclidean_span_on_both_axes() {
        let mut input = TouchInput::new();
        relay(&mut input, touch(1, TouchPhase::Started, 0., 0.));
        relay(&mut input, touch(2, TouchPhase::Started, 3., 4.));
        let events = relay(&mut input, touch(2, TouchPhase::Moved, 6., 8.));
        assert_eq!(pinch(&events).delta, 1.);
        assert_eq!(pinch(&events).position, point(px(3.), px(4.)));
    }

    #[test]
    fn either_contact_ends_or_cancels_the_pinch_without_a_tap() {
        for id in [1, 2] {
            for phase in [TouchPhase::Ended, TouchPhase::Cancelled] {
                let mut input = TouchInput::new();
                start_pair(&mut input);
                let x = if id == 1 { 0. } else { 10. };
                let events = relay(&mut input, touch(id, phase, x, 0.));
                assert_eq!(pinch(&events).phase, phase);
                assert_eq!(pinch(&events).delta, 0.);
                assert_eq!(pinch(&events).position, point(px(5.), px(0.)));
                let remaining = if id == 1 { 2 } else { 1 };
                assert!(relay(&mut input, touch(remaining, TouchPhase::Moved, 30., 0.)).is_empty());
                assert!(relay(&mut input, touch(remaining, TouchPhase::Ended, 30., 0.)).is_empty());
            }
        }
    }

    #[test]
    fn release_delivers_its_last_scale_before_ending() {
        let mut input = TouchInput::new();
        start_pair(&mut input);
        let events = relay(&mut input, touch(2, TouchPhase::Ended, 20., 0.));
        let [PlatformInput::Pinch(moved), PlatformInput::Pinch(ended)] = events.as_slice()
        else {
            panic!("expected final movement and end");
        };
        assert_eq!(moved.phase, TouchPhase::Moved);
        assert_eq!(moved.delta, 1.);
        assert_eq!(ended.phase, TouchPhase::Ended);
        assert_eq!(ended.delta, 0.);
        assert_eq!(ended.position, point(px(10.), px(0.)));
    }

    #[test]
    fn coincident_contacts_rebase_without_infinite_scale() {
        let mut input = TouchInput::new();
        relay(&mut input, touch(1, TouchPhase::Started, 0., 0.));
        relay(&mut input, touch(2, TouchPhase::Started, 0., 0.));
        let zero = relay(&mut input, touch(2, TouchPhase::Moved, 0., 0.));
        assert_eq!(pinch(&zero).delta, 0.);
        let baseline = relay(&mut input, touch(2, TouchPhase::Moved, 10., 0.));
        assert_eq!(pinch(&baseline).delta, 0.);
        let expansion = relay(&mut input, touch(2, TouchPhase::Moved, 20., 0.));
        assert_eq!(pinch(&expansion).delta, 1.);
    }

    #[test]
    fn extra_contacts_do_not_replace_the_pair_or_restart_pan() {
        let mut input = TouchInput::new();
        start_pair(&mut input);
        assert!(relay(&mut input, touch(3, TouchPhase::Started, 500., 0.)).is_empty());
        assert!(relay(&mut input, touch(3, TouchPhase::Moved, 700., 0.)).is_empty());
        let events = relay(&mut input, touch(2, TouchPhase::Moved, 20., 0.));
        assert_eq!(pinch(&events).delta, 1.);
        relay(&mut input, touch(1, TouchPhase::Ended, 0., 0.));
        assert!(relay(&mut input, touch(4, TouchPhase::Started, 800., 0.)).is_empty());
        for id in [2, 3, 4] {
            assert!(relay(&mut input, touch(id, TouchPhase::Cancelled, 0., 0.)).is_empty());
        }
        let events = relay(&mut input, touch(5, TouchPhase::Started, 1., 2.));
        assert!(matches!(events.as_slice(), [PlatformInput::Touch(event)]
            if event.id == TouchId(5) && event.phase == TouchPhase::Started));
    }

    #[test]
    fn unknown_contacts_and_duplicate_starts_do_not_change_pair() {
        let mut input = TouchInput::new();
        start_pair(&mut input);
        assert!(relay(&mut input, touch(99, TouchPhase::Moved, 30., 0.)).is_empty());
        assert!(relay(&mut input, touch(99, TouchPhase::Ended, 30., 0.)).is_empty());
        assert!(relay(&mut input, touch(1, TouchPhase::Started, 30., 0.)).is_empty());
        let events = relay(&mut input, touch(2, TouchPhase::Moved, 20., 0.));
        assert_eq!(pinch(&events).delta, 1.);
    }
}
