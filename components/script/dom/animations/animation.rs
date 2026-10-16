/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use dom_struct::dom_struct;
use js::context::{JSContext, NoGC};
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::AnimationBinding::{
    AnimationMethods, AnimationPlayState, AnimationReplaceState,
};
use script_bindings::codegen::GenericBindings::AnimationTimelineBinding::AnimationTimelineMethods;
use script_bindings::codegen::GenericBindings::DocumentBinding::DocumentMethods;
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::error::{Error, Fallible};
use script_bindings::inheritance::Castable;
use script_bindings::num::Finite;
use script_bindings::refcounted::Trusted;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::root::DomRoot;
use script_bindings::str::DOMString;
use style::animation::{AnimationSetKey, ScriptAnimationId};
use stylo_atoms::Atom;

use crate::dom::animationeffect::AnimationEffect;
use crate::dom::animationplaybackevent::AnimationPlaybackEvent;
use crate::dom::animations::keyframeeffect::KeyframeEffect;
use crate::dom::animations::timing;
use crate::dom::bindings::codegen::Bindings::AnimationPlaybackEventBinding::AnimationPlaybackEventInit;
use crate::dom::bindings::codegen::Bindings::EventBinding::EventInit;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, MutNullableDom};
use crate::dom::document::{Document, RenderingUpdateReason};
use crate::dom::element::Element;
use crate::dom::event::Event;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::node::{Node, NodeDamage};
use crate::dom::promise::{Promise, RootedPromise, TracedPromise};
use crate::dom::types::{AnimationTimeline, DocumentTimeline};
use crate::dom::window::Window;
use crate::realms::enter_auto_realm;
use crate::runtime::job_queue::MicrotaskRunnable;

/// Position in the global animation list, assigned at creation.
///
/// <https://drafts.csswg.org/web-animations-1/#global-animation-list>
static NEXT_SEQUENCE_NUMBER: AtomicU64 = AtomicU64::new(1);

/// A scheduled "pending play task" or "pending pause task". The task runs
/// on the next animation frame, when the timeline provides the ready time.
#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
enum PendingTask {
    Play,
    Pause,
}

/// <https://drafts.csswg.org/web-animations-1/#animation-playback-event-types>
#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
pub(crate) enum PlaybackEventType {
    Finish,
    Cancel,
}

impl PlaybackEventType {
    fn atom(self) -> Atom {
        match self {
            PlaybackEventType::Finish => Atom::from("finish"),
            PlaybackEventType::Cancel => Atom::from("cancel"),
        }
    }
}

/// <https://drafts.csswg.org/web-animations-1/#animation>
#[dom_struct]
pub(crate) struct Animation {
    event_target: EventTarget,

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-id>
    id: DomRefCell<DOMString>,

    /// <https://drafts.csswg.org/web-animations-1/#timeline>
    timeline: MutNullableDom<AnimationTimeline>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-associated-effect>
    associated_effect: MutNullableDom<AnimationEffect>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-start-time>
    start_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-hold-time>
    hold_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-previous-current-time>
    previous_current_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-playback-rate>
    playback_rate: Cell<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#pending-playback-rate>
    pending_playback_rate: Cell<Option<f64>>,

    /// The pending play or pause task, if one is scheduled.
    pending_task: Cell<Option<PendingTask>>,

    /// <https://drafts.csswg.org/web-animations-1/#current-ready-promise>
    ready: DomRefCell<TracedPromise>,

    /// <https://drafts.csswg.org/web-animations-1/#current-finished-promise>
    finished: DomRefCell<TracedPromise>,

    /// Whether a microtask to run the finish notification steps is queued.
    finish_notification_queued: Cell<bool>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-replace-state>
    replace_state: Cell<AnimationReplaceState>,

    /// Position in the global animation list.
    sequence_number: u64,

    /// The element whose stylo animation set currently holds this
    /// animation's effect, if any.
    sampled_target: MutNullableDom<Element>,

    /// The `KeyframeEffect::generation()` the stylo entry was built from.
    sampled_generation: Cell<u64>,

    /// The progress last written into the stylo entry.
    sampled_progress: Cell<Option<f64>>,
}

impl Animation {
    fn new_inherited(ready: &RootedPromise, finished: &RootedPromise) -> Self {
        Self {
            event_target: EventTarget::new_inherited(),
            id: DomRefCell::new(DOMString::new()),
            timeline: Default::default(),
            associated_effect: Default::default(),
            start_time: Cell::new(None),
            hold_time: Cell::new(None),
            previous_current_time: Cell::new(None),
            playback_rate: Cell::new(1.0),
            pending_playback_rate: Cell::new(None),
            pending_task: Cell::new(None),
            ready: DomRefCell::new(ready.to_traced()),
            finished: DomRefCell::new(finished.to_traced()),
            finish_notification_queued: Cell::new(false),
            replace_state: Cell::new(AnimationReplaceState::Active),
            sequence_number: NEXT_SEQUENCE_NUMBER.fetch_add(1, Ordering::Relaxed),
            sampled_target: Default::default(),
            sampled_generation: Cell::new(0),
            sampled_progress: Cell::new(None),
        }
    }

    fn new_with_proto(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        let ready = Promise::new(cx, global);
        let finished = Promise::new(cx, global);
        let animation = reflect_dom_object_with_proto(
            cx,
            Box::new(Self::new_inherited(&ready, &finished)),
            global,
            proto,
        );
        // The current ready promise is initially resolved with the
        // animation itself; the finished promise stays pending.
        ready.resolve_native(cx, &*animation);
        animation
    }

    /// Position in the global animation list; `getAnimations()` sorts by it.
    #[expect(dead_code, reason = "read by getAnimations(), which is not implemented yet")]
    pub(crate) fn sequence_number(&self) -> u64 {
        self.sequence_number
    }

    pub(crate) fn playback_rate(&self) -> f64 {
        self.playback_rate.get()
    }

    /// This animation's id in stylo's animation sets.
    fn script_animation_id(&self) -> ScriptAnimationId {
        ScriptAnimationId(self.sequence_number)
    }

    /// Writes the effect's current iteration progress into stylo's
    /// animation set for the target, starting the stylo animation when the
    /// set does not hold it yet or the keyframes changed, and dirties the
    /// target when the sampled value moved. Called by the manager after
    /// every tick while the animation is tracked.
    pub(crate) fn update_effect_sampling(&self, document: &Document, no_gc: &NoGC) {
        let effect = self
            .associated_effect
            .get()
            .and_then(|effect| DomRoot::downcast::<KeyframeEffect>(effect));
        let target = effect.as_ref().and_then(|effect| effect.target());
        let (Some(effect), Some(target)) = (effect, target) else {
            self.stop_sampling(document, no_gc);
            return;
        };
        // A detached target is never restyled; stylo would drop the entry
        // and we would re-add it every tick. Resume when it is back.
        if !target.upcast::<Node>().is_connected() {
            self.stop_sampling(document, no_gc);
            return;
        }

        // A new target or new keyframes: drop the old stylo entry first.
        let generation = effect.generation();
        if let Some(previous) = self.sampled_target.get() {
            if !std::ptr::eq(&*previous, &*target) || self.sampled_generation.get() != generation {
                self.stop_sampling(document, no_gc);
            }
        }

        let timing = effect.upcast::<AnimationEffect>().timing();
        let progress =
            timing::compute(&timing, self.current_time(), self.playback_rate.get()).progress;
        let node = target.upcast::<Node>();
        let key = AnimationSetKey::new_for_non_pseudo(node.to_opaque());
        let id = self.script_animation_id();
        let sets = document.animation_manager().sets();
        let mut changed = self.sampled_progress.get() != progress;
        if !sets.set_script_animation_progress(&key, id, progress) {
            // Not in the set yet (or stylo dropped it, e.g. the node left
            // the document): push the keyframes; they resolve on restyle.
            sets.start_script_animation(
                &key,
                effect.to_script_animation(id, document, timing.iteration_duration / 1000.0),
            );
            sets.set_script_animation_progress(&key, id, progress);
            changed = true;
        }
        self.sampled_target.set(Some(&*target));
        self.sampled_generation.set(generation);
        self.sampled_progress.set(progress);
        if changed {
            node.dirty(no_gc, NodeDamage::Style);
        }
    }

    /// Removes this animation from stylo's set for its last target and
    /// restyles the target so its style reverts.
    pub(crate) fn stop_sampling(&self, document: &Document, no_gc: &NoGC) {
        let Some(target) = self.sampled_target.get() else {
            return;
        };
        self.sampled_target.set(None);
        self.sampled_progress.set(None);
        let node = target.upcast::<Node>();
        let key = AnimationSetKey::new_for_non_pseudo(node.to_opaque());
        document
            .animation_manager()
            .sets()
            .cancel_script_animation(&key, self.script_animation_id());
        node.dirty(no_gc, NodeDamage::Style);
    }

    /// The document whose animation manager ticks this animation.
    ///
    /// <https://drafts.csswg.org/web-animations-1/#animation-document-for-timing>
    fn document_for_timing(&self) -> Option<DomRoot<Document>> {
        let timeline = self.timeline.get()?;
        timeline.downcast::<DocumentTimeline>()?;
        Some(timeline.global().as_window().Document())
    }

    /// The current time of the associated timeline, in milliseconds;
    /// `None` when there is no timeline or it is inactive.
    fn timeline_time(&self) -> Option<f64> {
        self.timeline
            .get()
            .and_then(|timeline| timeline.GetCurrentTime())
            .map(|time| *time)
    }

    /// <https://drafts.csswg.org/web-animations-1/#the-current-time-of-an-animation>
    pub(crate) fn current_time(&self) -> Option<f64> {
        if let Some(hold_time) = self.hold_time.get() {
            return Some(hold_time);
        }
        self.unconstrained_current_time()
    }

    /// The current time calculated with an unresolved hold time.
    fn unconstrained_current_time(&self) -> Option<f64> {
        let timeline_time = self.timeline_time()?;
        let start_time = self.start_time.get()?;
        Some((timeline_time - start_time) * self.playback_rate.get())
    }

    /// <https://drafts.csswg.org/web-animations-1/#effective-playback-rate>
    fn effective_playback_rate(&self) -> f64 {
        self.pending_playback_rate
            .get()
            .unwrap_or_else(|| self.playback_rate.get())
    }

    /// <https://drafts.csswg.org/web-animations-1/#apply-any-pending-playback-rate>
    fn apply_any_pending_playback_rate(&self) {
        if let Some(rate) = self.pending_playback_rate.take() {
            self.playback_rate.set(rate);
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#associated-effect-end>
    fn associated_effect_end(&self) -> f64 {
        self.associated_effect
            .get()
            .map_or(0.0, |effect| effect.end_time())
    }

    /// <https://drafts.csswg.org/web-animations-1/#play-states>
    pub(crate) fn play_state(&self) -> AnimationPlayState {
        let current_time = self.current_time();
        let start_time = self.start_time.get();
        let pending_task = self.pending_task.get();
        if current_time.is_none() && start_time.is_none() && pending_task.is_none() {
            return AnimationPlayState::Idle;
        }
        if pending_task == Some(PendingTask::Pause) ||
            (start_time.is_none() && pending_task != Some(PendingTask::Play))
        {
            return AnimationPlayState::Paused;
        }
        if let Some(current_time) = current_time {
            let rate = self.effective_playback_rate();
            let end = self.associated_effect_end();
            if (rate > 0.0 && current_time >= end) || (rate < 0.0 && current_time <= 0.0) {
                return AnimationPlayState::Finished;
            }
        }
        AnimationPlayState::Running
    }

    /// Whether the manager must keep ticking for this animation. Without an
    /// active timeline neither a pending task nor the current time can make
    /// progress, so there is nothing to tick.
    pub(crate) fn needs_ticks(&self) -> bool {
        self.timeline_time().is_some() &&
            (self.pending_task.get().is_some() ||
                self.play_state() == AnimationPlayState::Running)
    }

    /// Whether the effect is in effect or current, which is what keeps a
    /// finished animation observable through `getAnimations()`.
    ///
    /// <https://drafts.csswg.org/web-animations-1/#relevant>
    pub(crate) fn is_relevant(&self) -> bool {
        let Some(effect) = self.associated_effect.get() else {
            return false;
        };
        let timing = effect.timing();
        let local_time = self.current_time();
        let rate = self.playback_rate.get();
        let phase = timing::phase(&timing, local_time, rate);
        let in_effect = timing::active_time(&timing, local_time, phase).is_some();
        let current = match phase {
            timing::Phase::Active => self.play_state() != AnimationPlayState::Finished,
            timing::Phase::Before => rate > 0.0,
            timing::Phase::After => rate < 0.0,
            timing::Phase::Idle => false,
        };
        in_effect || current
    }

    fn replace_ready_promise(&self, cx: &mut JSContext) {
        let promise = Promise::new(cx, &self.global());
        *self.ready.borrow_mut() = promise.to_traced();
    }

    fn resolve_ready_promise(&self, cx: &mut JSContext) {
        let promise = self.ready.borrow().root(cx);
        promise.resolve_native(cx, self);
    }

    fn replace_finished_promise(&self, cx: &mut JSContext) {
        let promise = Promise::new(cx, &self.global());
        *self.finished.borrow_mut() = promise.to_traced();
    }

    /// Hands the animation to the document's manager, which ticks it from
    /// then on. Idempotent.
    fn register_with_document(&self) {
        if let Some(document) = self.document_for_timing() {
            document.animation_manager().register_script_animation(self);
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#setting-the-timeline>
    fn set_the_timeline(&self, cx: &mut JSContext, new_timeline: Option<&AnimationTimeline>) {
        let old_timeline = self.timeline.get();
        if old_timeline.as_deref() == new_timeline {
            return;
        }
        self.timeline.set(new_timeline);
        if self.start_time.get().is_some() {
            self.hold_time.set(None);
        }
        self.update_finished_state(cx, false, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#setting-the-associated-effect>
    fn set_the_associated_effect(&self, cx: &mut JSContext, new_effect: Option<&AnimationEffect>) {
        let old_effect = self.associated_effect.get();
        if old_effect.as_deref() == new_effect {
            return;
        }
        // Steps 3 and 4: a pending task is rescheduled to run when the
        // animation is ready, which is the next frame either way.
        if let Some(new_effect) = new_effect {
            // Step 5. An effect belongs to at most one animation.
            if let Some(previous_animation) = new_effect.animation() {
                previous_animation.set_the_associated_effect(cx, None);
            }
        }
        if let Some(old_effect) = old_effect {
            old_effect.set_animation(None);
        }
        self.associated_effect.set(new_effect);
        if let Some(new_effect) = new_effect {
            new_effect.set_animation(Some(self));
        }
        self.update_finished_state(cx, false, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#silently-set-the-current-time>
    fn silently_set_the_current_time(&self, seek_time: Option<f64>) -> Fallible<()> {
        let Some(seek_time) = seek_time else {
            if self.current_time().is_some() {
                return Err(Error::Type(
                    c"currentTime may not be set to null on an animation with a resolved current time"
                        .to_owned(),
                ));
            }
            return Ok(());
        };
        let timeline_time = self.timeline_time();
        if self.hold_time.get().is_some() ||
            self.start_time.get().is_none() ||
            timeline_time.is_none() ||
            self.playback_rate.get() == 0.0
        {
            self.hold_time.set(Some(seek_time));
        } else if let Some(timeline_time) = timeline_time {
            self.start_time
                .set(Some(timeline_time - seek_time / self.playback_rate.get()));
        }
        if timeline_time.is_none() {
            self.start_time.set(None);
        }
        self.previous_current_time.set(None);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#set-the-current-time>
    fn set_the_current_time(&self, cx: &mut JSContext, seek_time: Option<f64>) -> Fallible<()> {
        self.silently_set_the_current_time(seek_time)?;
        if self.pending_task.get() == Some(PendingTask::Pause) {
            self.hold_time.set(seek_time);
            self.apply_any_pending_playback_rate();
            self.start_time.set(None);
            self.pending_task.set(None);
            self.resolve_ready_promise(cx);
        }
        self.register_with_document();
        self.update_finished_state(cx, true, false);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#set-the-start-time>
    fn set_the_start_time(&self, cx: &mut JSContext, new_start_time: Option<f64>) {
        let timeline_time = self.timeline_time();
        if timeline_time.is_none() && new_start_time.is_some() {
            self.hold_time.set(None);
        }
        let previous_current_time = self.current_time();
        self.apply_any_pending_playback_rate();
        self.start_time.set(new_start_time);
        if new_start_time.is_some() {
            if self.playback_rate.get() != 0.0 {
                self.hold_time.set(None);
            }
        } else {
            self.hold_time.set(previous_current_time);
        }
        if self.pending_task.take().is_some() {
            self.resolve_ready_promise(cx);
        }
        self.register_with_document();
        self.update_finished_state(cx, true, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#play-an-animation>
    pub(crate) fn play_an_animation(&self, cx: &mut JSContext, auto_rewind: bool) -> Fallible<()> {
        // Step 1.
        let aborted_pause = self.pending_task.get() == Some(PendingTask::Pause);
        // Step 2.
        let mut has_pending_ready_promise = false;
        // Step 3.
        let mut seek_time: Option<f64> = None;

        // Step 4.
        if auto_rewind {
            let rate = self.effective_playback_rate();
            let current_time = self.current_time();
            let end = self.associated_effect_end();
            if rate >= 0.0 && current_time.is_none_or(|time| time < 0.0 || time >= end) {
                seek_time = Some(0.0);
            } else if rate < 0.0 && current_time.is_none_or(|time| time <= 0.0 || time > end) {
                if end.is_infinite() {
                    return Err(Error::InvalidState(None));
                }
                seek_time = Some(end);
            }
        }

        // Step 5.
        if seek_time.is_none() && self.start_time.get().is_none() && self.current_time().is_none() {
            seek_time = Some(0.0);
        }

        // Steps 6 and 7. Document timelines are monotonically increasing,
        // so "has finite timeline" is always false here.
        if let Some(seek_time) = seek_time {
            self.hold_time.set(Some(seek_time));
        }

        // Step 8.
        if self.hold_time.get().is_some() {
            self.start_time.set(None);
        }

        // Step 9.
        if self.pending_task.take().is_some() {
            has_pending_ready_promise = true;
        }

        // Step 10.
        if self.hold_time.get().is_none() &&
            seek_time.is_none() &&
            !aborted_pause &&
            self.pending_playback_rate.get().is_none()
        {
            return Ok(());
        }

        // Step 11.
        if !has_pending_ready_promise {
            self.replace_ready_promise(cx);
        }

        // Step 12. The task runs on the next animation frame.
        self.pending_task.set(Some(PendingTask::Play));
        self.register_with_document();

        // Step 13.
        self.update_finished_state(cx, false, false);
        Ok(())
    }

    /// The pending play task of
    /// <https://drafts.csswg.org/web-animations-1/#play-an-animation>, step 12.
    fn run_pending_play_task(&self, cx: &mut JSContext, ready_time: f64) {
        debug_assert!(self.start_time.get().is_some() || self.hold_time.get().is_some());
        if let Some(hold_time) = self.hold_time.get() {
            self.apply_any_pending_playback_rate();
            let rate = self.playback_rate.get();
            let new_start_time = if rate == 0.0 {
                ready_time
            } else {
                ready_time - hold_time / rate
            };
            self.start_time.set(Some(new_start_time));
            if rate != 0.0 {
                self.hold_time.set(None);
            }
        } else if let (Some(start_time), Some(_)) =
            (self.start_time.get(), self.pending_playback_rate.get())
        {
            let current_time_to_match = (ready_time - start_time) * self.playback_rate.get();
            self.apply_any_pending_playback_rate();
            let rate = self.playback_rate.get();
            if rate == 0.0 {
                self.hold_time.set(Some(current_time_to_match));
            }
            let new_start_time = if rate == 0.0 {
                ready_time
            } else {
                ready_time - current_time_to_match / rate
            };
            self.start_time.set(Some(new_start_time));
        }
        self.resolve_ready_promise(cx);
        self.update_finished_state(cx, false, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#pause-an-animation>
    fn pause_an_animation(&self, cx: &mut JSContext) -> Fallible<()> {
        // Steps 1 and 2.
        if self.pending_task.get() == Some(PendingTask::Pause) ||
            self.play_state() == AnimationPlayState::Paused
        {
            return Ok(());
        }
        // Steps 3 to 5.
        let mut seek_time = None;
        if self.current_time().is_none() {
            if self.playback_rate.get() >= 0.0 {
                seek_time = Some(0.0);
            } else {
                let end = self.associated_effect_end();
                if end.is_infinite() {
                    return Err(Error::InvalidState(None));
                }
                seek_time = Some(end);
            }
        }
        // Step 6 (monotonically increasing timeline: set the hold time).
        if let Some(seek_time) = seek_time {
            self.hold_time.set(Some(seek_time));
        }
        // Steps 7 to 9.
        let mut has_pending_ready_promise = false;
        if self.pending_task.get() == Some(PendingTask::Play) {
            self.pending_task.set(None);
            has_pending_ready_promise = true;
        }
        if !has_pending_ready_promise {
            self.replace_ready_promise(cx);
        }
        // Step 10. The task runs on the next animation frame.
        self.pending_task.set(Some(PendingTask::Pause));
        self.register_with_document();
        // Step 11.
        self.update_finished_state(cx, false, false);
        Ok(())
    }

    /// The pending pause task of
    /// <https://drafts.csswg.org/web-animations-1/#pause-an-animation>, step 10.
    fn run_pending_pause_task(&self, cx: &mut JSContext, ready_time: f64) {
        if let (Some(start_time), None) = (self.start_time.get(), self.hold_time.get()) {
            self.hold_time
                .set(Some((ready_time - start_time) * self.playback_rate.get()));
        }
        self.apply_any_pending_playback_rate();
        self.start_time.set(None);
        self.resolve_ready_promise(cx);
        self.update_finished_state(cx, false, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#set-the-playback-rate>
    fn set_the_playback_rate(&self, cx: &mut JSContext, new_playback_rate: f64) {
        self.pending_playback_rate.set(None);
        let previous_time = self.current_time();
        self.playback_rate.set(new_playback_rate);
        if let Some(previous_time) = previous_time {
            // Cannot fail: the seek time is resolved.
            let _ = self.set_the_current_time(cx, Some(previous_time));
        }
        self.update_finished_state(cx, true, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#seamlessly-update-the-playback-rate>
    fn seamlessly_update_the_playback_rate(
        &self,
        cx: &mut JSContext,
        new_playback_rate: f64,
    ) -> Fallible<()> {
        let previous_play_state = self.play_state();
        self.pending_playback_rate.set(Some(new_playback_rate));
        if self.pending_task.get().is_some() {
            return Ok(());
        }
        match previous_play_state {
            AnimationPlayState::Idle | AnimationPlayState::Paused => {
                self.apply_any_pending_playback_rate();
            },
            _ if self.current_time().is_none() => {
                self.apply_any_pending_playback_rate();
            },
            AnimationPlayState::Finished => {
                if let (Some(timeline_time), Some(unconstrained_current_time)) =
                    (self.timeline_time(), self.unconstrained_current_time())
                {
                    let start_time = if new_playback_rate == 0.0 {
                        timeline_time
                    } else {
                        timeline_time - unconstrained_current_time / new_playback_rate
                    };
                    self.start_time.set(Some(start_time));
                }
                self.apply_any_pending_playback_rate();
                self.update_finished_state(cx, false, false);
            },
            AnimationPlayState::Running => {
                self.play_an_animation(cx, false)?;
            },
        }
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#reverse-an-animation>
    fn reverse_an_animation(&self, cx: &mut JSContext) -> Fallible<()> {
        if self.timeline_time().is_none() {
            return Err(Error::InvalidState(None));
        }
        let original_pending_playback_rate = self.pending_playback_rate.get();
        self.pending_playback_rate
            .set(Some(-self.effective_playback_rate()));
        if let Err(error) = self.play_an_animation(cx, true) {
            self.pending_playback_rate
                .set(original_pending_playback_rate);
            return Err(error);
        }
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#finish-an-animation>
    fn finish_an_animation(&self, cx: &mut JSContext) -> Fallible<()> {
        let effective_rate = self.effective_playback_rate();
        let end = self.associated_effect_end();
        if effective_rate == 0.0 || (effective_rate > 0.0 && end.is_infinite()) {
            return Err(Error::InvalidState(None));
        }
        self.apply_any_pending_playback_rate();
        let rate = self.playback_rate.get();
        let limit = if rate > 0.0 { end } else { 0.0 };
        // Cannot fail: the seek time is resolved.
        let _ = self.silently_set_the_current_time(Some(limit));
        if self.start_time.get().is_none() {
            if let Some(timeline_time) = self.timeline_time() {
                self.start_time.set(Some(timeline_time - limit / rate));
            }
        }
        if self.start_time.get().is_some() {
            match self.pending_task.get() {
                Some(PendingTask::Pause) => {
                    self.hold_time.set(None);
                    self.pending_task.set(None);
                    self.resolve_ready_promise(cx);
                },
                Some(PendingTask::Play) => {
                    self.pending_task.set(None);
                    self.resolve_ready_promise(cx);
                },
                None => {},
            }
        }
        self.register_with_document();
        self.update_finished_state(cx, true, true);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#reset-an-animations-pending-tasks>
    fn reset_pending_tasks(&self, cx: &mut JSContext) {
        if self.pending_task.take().is_none() {
            return;
        }
        self.apply_any_pending_playback_rate();
        let ready = self.ready.borrow().root(cx);
        ready.reject_error(cx, Error::Abort(None));
        ready.set_promise_is_handled(cx);
        let resolved = Promise::new_resolved(cx, &self.global(), self);
        *self.ready.borrow_mut() = resolved.to_traced();
    }

    /// <https://drafts.csswg.org/web-animations-1/#cancel-an-animation>
    fn cancel_an_animation(&self, cx: &mut JSContext) {
        if self.play_state() != AnimationPlayState::Idle {
            self.reset_pending_tasks(cx);
            let finished = self.finished.borrow().root(cx);
            finished.reject_error(cx, Error::Abort(None));
            finished.set_promise_is_handled(cx);
            self.replace_finished_promise(cx);
            let timeline_time = self.timeline_time();
            self.queue_playback_event(
                PlaybackEventType::Cancel,
                None,
                timeline_time,
                timeline_time,
            );
        }
        self.hold_time.set(None);
        self.start_time.set(None);
    }

    /// <https://drafts.csswg.org/web-animations-1/#update-an-animations-finished-state>
    pub(crate) fn update_finished_state(
        &self,
        cx: &mut JSContext,
        did_seek: bool,
        synchronously_notify: bool,
    ) {
        // Step 1.
        let unconstrained_current_time = if did_seek {
            self.current_time()
        } else {
            self.unconstrained_current_time()
        };

        // Step 2.
        if let (Some(unconstrained_current_time), Some(_), None) = (
            unconstrained_current_time,
            self.start_time.get(),
            self.pending_task.get(),
        ) {
            let rate = self.playback_rate.get();
            let end = self.associated_effect_end();
            if rate > 0.0 && unconstrained_current_time >= end {
                let hold_time = if did_seek {
                    unconstrained_current_time
                } else {
                    self.previous_current_time
                        .get()
                        .map_or(end, |previous| previous.max(end))
                };
                self.hold_time.set(Some(hold_time));
            } else if rate < 0.0 && unconstrained_current_time <= 0.0 {
                let hold_time = if did_seek {
                    unconstrained_current_time
                } else {
                    self.previous_current_time
                        .get()
                        .map_or(0.0, |previous| previous.min(0.0))
                };
                self.hold_time.set(Some(hold_time));
            } else if rate != 0.0 {
                if let Some(timeline_time) = self.timeline_time() {
                    if did_seek {
                        if let Some(hold_time) = self.hold_time.get() {
                            self.start_time.set(Some(timeline_time - hold_time / rate));
                        }
                    }
                    self.hold_time.set(None);
                }
            }
        }

        // Step 3.
        self.previous_current_time.set(self.current_time());

        // Steps 4 and 5.
        let current_finished_state = self.play_state() == AnimationPlayState::Finished;
        if current_finished_state && self.finished.borrow().is_pending() {
            if synchronously_notify {
                self.finish_notification_queued.set(false);
                self.finish_notification_steps(cx);
            } else if !self.finish_notification_queued.replace(true) {
                self.global().enqueue_microtask(
                    cx,
                    Box::new(FinishNotificationMicrotask {
                        animation: Dom::from_ref(self),
                    }),
                );
            }
        }

        // Step 6.
        if !current_finished_state && self.finished.borrow().is_fulfilled() {
            self.replace_finished_promise(cx);
        }

        // Anything that made the animation need ticks again (a longer
        // effect from updateTiming(), a new effect or timeline, a rate
        // change on a finished animation) must put it back on the manager's
        // list; registering is idempotent.
        if self.needs_ticks() {
            self.register_with_document();
        }
    }

    /// The "finish notification steps" of
    /// <https://drafts.csswg.org/web-animations-1/#update-an-animations-finished-state>.
    fn finish_notification_steps(&self, cx: &mut JSContext) {
        if self.play_state() != AnimationPlayState::Finished {
            return;
        }
        let finished = self.finished.borrow().root(cx);
        finished.resolve_native(cx, self);
        let current_time = self.current_time();
        let timeline_time = self.timeline_time();
        let scheduled_time = self.animation_time_to_timeline_time(self.associated_effect_end());
        self.queue_playback_event(
            PlaybackEventType::Finish,
            current_time,
            timeline_time,
            scheduled_time,
        );
    }

    /// <https://drafts.csswg.org/web-animations-1/#convert-an-animation-time-to-timeline-time>
    ///
    /// The default document timeline has an origin time of zero, so this
    /// is also the origin-relative time the pending event queue sorts by.
    fn animation_time_to_timeline_time(&self, time: f64) -> Option<f64> {
        if time.is_infinite() {
            return None;
        }
        let rate = self.playback_rate.get();
        if rate == 0.0 {
            return None;
        }
        let start_time = self.start_time.get()?;
        self.timeline_time()?;
        Some(time / rate + start_time)
    }

    /// Appends a playback event to the document's pending animation event
    /// queue, or dispatches it from a task when there is no document.
    fn queue_playback_event(
        &self,
        event_type: PlaybackEventType,
        current_time: Option<f64>,
        timeline_time: Option<f64>,
        scheduled_time: Option<f64>,
    ) {
        match self.document_for_timing() {
            Some(document) => {
                document.animation_manager().queue_script_animation_event(
                    self,
                    event_type,
                    current_time,
                    timeline_time,
                    scheduled_time,
                );
                document.add_rendering_update_reason(
                    RenderingUpdateReason::AnimationPlaybackEventQueued,
                );
            },
            None => {
                let animation = Trusted::new(self);
                self.global()
                    .task_manager()
                    .dom_manipulation_task_source()
                    .queue(task!(dispatch_animation_playback_event: move |cx| {
                        let animation = animation.root();
                        animation.dispatch_playback_event(cx, event_type, current_time, timeline_time);
                    }));
            },
        }
    }

    /// Fires a `finish` or `cancel` event at this animation.
    pub(crate) fn dispatch_playback_event(
        &self,
        cx: &mut JSContext,
        event_type: PlaybackEventType,
        current_time: Option<f64>,
        timeline_time: Option<f64>,
    ) {
        let global = self.global();
        let window = global.as_window();
        let init = AnimationPlaybackEventInit {
            parent: EventInit {
                bubbles: false,
                cancelable: false,
                composed: false,
            },
            currentTime: current_time.and_then(Finite::new),
            timelineTime: timeline_time.and_then(Finite::new),
        };
        let event = AnimationPlaybackEvent::new(cx, window, event_type.atom(), &init);
        event.upcast::<Event>().fire(cx, self.upcast());
    }

    /// Advances this animation on a tick of `document`'s timeline: runs the
    /// pending play or pause task, with the animation's own timeline time as
    /// the ready time, and updates the finished state. Returns false once
    /// the manager of `document` may forget the animation: it uses another
    /// timeline, it has nothing to tick, or it is finished and not relevant.
    pub(crate) fn update_for_timeline_tick(&self, cx: &mut JSContext, document: &Document) -> bool {
        if self.document_for_timing().as_deref() != Some(document) {
            return false;
        }
        // Pending tasks run only with an active timeline, and then at its
        // current time: "play an animation" is not run without one.
        if let (Some(ready_time), Some(task)) = (self.timeline_time(), self.pending_task.get()) {
            self.pending_task.set(None);
            match task {
                PendingTask::Play => self.run_pending_play_task(cx, ready_time),
                PendingTask::Pause => self.run_pending_pause_task(cx, ready_time),
            }
        }
        self.update_finished_state(cx, false, false);
        match self.play_state() {
            AnimationPlayState::Idle => false,
            AnimationPlayState::Finished => self.is_relevant(),
            // A paused animation in effect is not ticked but its value still
            // has to be sampled, so it stays tracked while it is relevant.
            _ => self.needs_ticks() || self.is_relevant(),
        }
    }
}

/// The microtask that runs the finish notification steps.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct FinishNotificationMicrotask {
    animation: Dom<Animation>,
}

impl MicrotaskRunnable for FinishNotificationMicrotask {
    fn handler(&self, cx: &mut JSContext) {
        let animation = DomRoot::from_ref(&*self.animation);
        if !animation.finish_notification_queued.replace(false) {
            // A synchronous notification already ran these steps.
            return;
        }
        let global = animation.global();
        let mut realm = enter_auto_realm(cx, &*global);
        let cx = &mut realm.current_realm();
        animation.finish_notification_steps(cx);
    }
}

impl AnimationMethods<crate::DomTypeHolder> for Animation {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-animation>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        effect: Option<&AnimationEffect>,
        timeline: Option<Option<&AnimationTimeline>>,
    ) -> DomRoot<Self> {
        let animation = Animation::new_with_proto(cx, window.upcast(), proto);
        match timeline {
            // An explicit null means no timeline; only a missing argument
            // defaults to the document timeline.
            Some(timeline) => animation.set_the_timeline(cx, timeline),
            None => {
                let document = window.Document();
                let timeline = document.Timeline();
                animation.set_the_timeline(cx, Some(timeline.upcast()));
            },
        }
        animation.set_the_associated_effect(cx, effect);
        animation
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-id>
    fn Id(&self) -> DOMString {
        self.id.borrow().clone()
    }

    fn SetId(&self, id: DOMString) {
        *self.id.borrow_mut() = id;
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-effect>
    fn GetEffect(&self) -> Option<DomRoot<AnimationEffect>> {
        self.associated_effect.get()
    }

    fn SetEffect(&self, cx: &mut JSContext, effect: Option<&AnimationEffect>) {
        self.set_the_associated_effect(cx, effect);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-timeline>
    fn GetTimeline(&self) -> Option<DomRoot<AnimationTimeline>> {
        self.timeline.get()
    }

    fn SetTimeline(&self, cx: &mut JSContext, timeline: Option<&AnimationTimeline>) {
        self.set_the_timeline(cx, timeline);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-starttime>
    fn GetStartTime(&self) -> Option<Finite<f64>> {
        self.start_time.get().and_then(Finite::new)
    }

    fn SetStartTime(&self, cx: &mut JSContext, start_time: Option<Finite<f64>>) {
        self.set_the_start_time(cx, start_time.map(|time| *time));
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-currenttime>
    fn GetCurrentTime(&self) -> Option<Finite<f64>> {
        self.current_time().and_then(Finite::new)
    }

    fn SetCurrentTime(
        &self,
        cx: &mut JSContext,
        current_time: Option<Finite<f64>>,
    ) -> Fallible<()> {
        self.set_the_current_time(cx, current_time.map(|time| *time))
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-playbackrate>
    fn PlaybackRate(&self) -> Finite<f64> {
        Finite::wrap(self.playback_rate.get())
    }

    fn SetPlaybackRate(&self, cx: &mut JSContext, playback_rate: Finite<f64>) {
        self.set_the_playback_rate(cx, *playback_rate);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-playstate>
    fn PlayState(&self) -> AnimationPlayState {
        self.play_state()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-replacestate>
    fn ReplaceState(&self) -> AnimationReplaceState {
        self.replace_state.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-pending>
    fn Pending(&self) -> bool {
        self.pending_task.get().is_some()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-ready>
    fn Ready(&self, cx: &mut JSContext) -> RootedPromise {
        self.ready.borrow().root(cx)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-finished>
    fn Finished(&self, cx: &mut JSContext) -> RootedPromise {
        self.finished.borrow().root(cx)
    }

    // <https://drafts.csswg.org/web-animations-1/#dom-animation-onfinish>
    event_handler!(finish, GetOnfinish, SetOnfinish);

    // <https://drafts.csswg.org/web-animations-1/#dom-animation-oncancel>
    event_handler!(cancel, GetOncancel, SetOncancel);

    // <https://drafts.csswg.org/web-animations-1/#dom-animation-onremove>
    event_handler!(remove, GetOnremove, SetOnremove);

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-cancel>
    fn Cancel(&self, cx: &mut JSContext) {
        self.cancel_an_animation(cx);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-finish>
    fn Finish(&self, cx: &mut JSContext) -> Fallible<()> {
        self.finish_an_animation(cx)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-play>
    fn Play(&self, cx: &mut JSContext) -> Fallible<()> {
        self.play_an_animation(cx, true)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-pause>
    fn Pause(&self, cx: &mut JSContext) -> Fallible<()> {
        self.pause_an_animation(cx)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-updateplaybackrate>
    fn UpdatePlaybackRate(&self, cx: &mut JSContext, playback_rate: Finite<f64>) -> Fallible<()> {
        self.seamlessly_update_the_playback_rate(cx, *playback_rate)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-reverse>
    fn Reverse(&self, cx: &mut JSContext) -> Fallible<()> {
        self.reverse_an_animation(cx)
    }
}
