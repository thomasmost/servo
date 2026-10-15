/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The Web Animations timing model as pure functions, shared by
//! `AnimationEffect::getComputedTiming()` and `Animation`.
//!
//! <https://drafts.csswg.org/web-animations-1/#timing-model>

use style::values::computed::easing::TimingFunction;
use style::values::generics::easing::BeforeFlag;

/// <https://drafts.csswg.org/web-animations-1/#fill-mode>, with `auto`
/// already resolved to `none`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Fill {
    None,
    Forwards,
    Backwards,
    Both,
}

/// <https://drafts.csswg.org/web-animations-1/#playback-direction>
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Direction {
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}

/// The timing properties of an animation effect with `auto` values
/// resolved. Times are in milliseconds.
#[derive(Clone, Debug)]
pub(crate) struct Timing {
    pub(crate) start_delay: f64,
    pub(crate) end_delay: f64,
    pub(crate) fill: Fill,
    pub(crate) iteration_start: f64,
    pub(crate) iteration_count: f64,
    pub(crate) iteration_duration: f64,
    pub(crate) direction: Direction,
    pub(crate) easing: TimingFunction,
}

/// <https://drafts.csswg.org/web-animations-1/#animation-effect-phases-and-states>
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Phase {
    Before,
    Active,
    After,
    Idle,
}

/// The values `getComputedTiming()` reports on top of the specified ones.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ComputedTiming {
    pub(crate) end_time: f64,
    pub(crate) active_duration: f64,
    pub(crate) local_time: Option<f64>,
    /// The transformed (iteration) progress, `None` when the effect is
    /// not in effect.
    pub(crate) progress: Option<f64>,
    pub(crate) current_iteration: Option<f64>,
}

/// <https://drafts.csswg.org/web-animations-1/#active-duration>
pub(crate) fn active_duration(timing: &Timing) -> f64 {
    // The product is undefined when one factor is zero and the other
    // infinite, so the spec special-cases zero.
    if timing.iteration_duration == 0.0 || timing.iteration_count == 0.0 {
        return 0.0;
    }
    timing.iteration_duration * timing.iteration_count
}

/// <https://drafts.csswg.org/web-animations-1/#end-time>
pub(crate) fn end_time(timing: &Timing) -> f64 {
    (timing.start_delay + active_duration(timing) + timing.end_delay).max(0.0)
}

/// <https://drafts.csswg.org/web-animations-1/#animation-effect-phases-and-states>
///
/// `playback_rate` is the associated animation's playback rate; a negative
/// value makes the animation direction "backwards".
pub(crate) fn phase(timing: &Timing, local_time: Option<f64>, playback_rate: f64) -> Phase {
    let Some(local_time) = local_time else {
        return Phase::Idle;
    };
    let end_time = end_time(timing);
    let before_active_boundary = timing.start_delay.min(end_time).max(0.0);
    let active_after_boundary = (timing.start_delay + active_duration(timing))
        .min(end_time)
        .max(0.0);
    let backwards = playback_rate < 0.0;
    if local_time < before_active_boundary || (backwards && local_time == before_active_boundary) {
        return Phase::Before;
    }
    if local_time > active_after_boundary || (!backwards && local_time == active_after_boundary) {
        return Phase::After;
    }
    Phase::Active
}

/// <https://drafts.csswg.org/web-animations-1/#calculating-the-active-time>
pub(crate) fn active_time(timing: &Timing, local_time: Option<f64>, phase: Phase) -> Option<f64> {
    let local_time = local_time?;
    match phase {
        Phase::Before => matches!(timing.fill, Fill::Backwards | Fill::Both)
            .then(|| (local_time - timing.start_delay).max(0.0)),
        Phase::Active => Some(local_time - timing.start_delay),
        Phase::After => matches!(timing.fill, Fill::Forwards | Fill::Both).then(|| {
            (local_time - timing.start_delay)
                .min(active_duration(timing))
                .max(0.0)
        }),
        Phase::Idle => None,
    }
}

/// <https://drafts.csswg.org/web-animations-1/#calculating-the-overall-progress>
fn overall_progress(timing: &Timing, active_time: f64, phase: Phase) -> f64 {
    let progress = if timing.iteration_duration == 0.0 {
        if phase == Phase::Before {
            0.0
        } else {
            timing.iteration_count
        }
    } else {
        active_time / timing.iteration_duration
    };
    progress + timing.iteration_start
}

/// <https://drafts.csswg.org/web-animations-1/#calculating-the-simple-iteration-progress>
fn simple_iteration_progress(
    timing: &Timing,
    overall_progress: f64,
    phase: Phase,
    active_time: f64,
) -> f64 {
    let progress = if overall_progress.is_infinite() {
        timing.iteration_start % 1.0
    } else {
        overall_progress % 1.0
    };
    if progress == 0.0
        && matches!(phase, Phase::Active | Phase::After)
        && active_time == active_duration(timing)
        && timing.iteration_count != 0.0
    {
        return 1.0;
    }
    progress
}

/// <https://drafts.csswg.org/web-animations-1/#calculating-the-current-iteration>
fn current_iteration(
    timing: &Timing,
    overall_progress: f64,
    simple_iteration_progress: f64,
    phase: Phase,
) -> f64 {
    if phase == Phase::After && timing.iteration_count.is_infinite() {
        return f64::INFINITY;
    }
    if simple_iteration_progress == 1.0 {
        return overall_progress.floor() - 1.0;
    }
    overall_progress.floor()
}

/// The "current direction" of
/// <https://drafts.csswg.org/web-animations-1/#calculating-the-directed-progress>:
/// true when forwards.
fn current_direction_is_forwards(timing: &Timing, current_iteration: f64) -> bool {
    match timing.direction {
        Direction::Normal => true,
        Direction::Reverse => false,
        Direction::Alternate | Direction::AlternateReverse => {
            if current_iteration.is_infinite() {
                return true;
            }
            let mut d = current_iteration;
            if timing.direction == Direction::AlternateReverse {
                d += 1.0;
            }
            d % 2.0 == 0.0
        },
    }
}

/// Runs the whole chain from local time to transformed progress.
///
/// <https://drafts.csswg.org/web-animations-1/#calculating-the-transformed-progress>
pub(crate) fn compute(
    timing: &Timing,
    local_time: Option<f64>,
    playback_rate: f64,
) -> ComputedTiming {
    let end_time = end_time(timing);
    let active_duration = active_duration(timing);
    let phase = phase(timing, local_time, playback_rate);
    let Some(active_time) = active_time(timing, local_time, phase) else {
        return ComputedTiming {
            end_time,
            active_duration,
            local_time,
            progress: None,
            current_iteration: None,
        };
    };
    let overall_progress = overall_progress(timing, active_time, phase);
    let simple_iteration_progress =
        simple_iteration_progress(timing, overall_progress, phase, active_time);
    let current_iteration =
        current_iteration(timing, overall_progress, simple_iteration_progress, phase);
    let forwards = current_direction_is_forwards(timing, current_iteration);
    let directed_progress = if forwards {
        simple_iteration_progress
    } else {
        1.0 - simple_iteration_progress
    };
    let before_flag =
        if (phase == Phase::Before && forwards) || (phase == Phase::After && !forwards) {
            BeforeFlag::Set
        } else {
            BeforeFlag::Unset
        };
    // The same tolerance stylo uses for CSS animations: a 200th of the
    // iteration (in seconds), bounded for zero-duration effects.
    let epsilon = if timing.iteration_duration > 0.0 {
        1.0 / (200.0 * timing.iteration_duration / 1000.0)
    } else {
        1.0 / 200.0
    };
    let progress = timing
        .easing
        .calculate_output(directed_progress, before_flag, epsilon);
    ComputedTiming {
        end_time,
        active_duration,
        local_time,
        progress: Some(progress),
        current_iteration: Some(current_iteration),
    }
}

#[cfg(test)]
mod tests {
    use style::values::generics::easing::TimingKeyword;

    use super::*;

    fn linear(duration: f64) -> Timing {
        Timing {
            start_delay: 0.0,
            end_delay: 0.0,
            fill: Fill::None,
            iteration_start: 0.0,
            iteration_count: 1.0,
            iteration_duration: duration,
            direction: Direction::Normal,
            easing: TimingFunction::Keyword(TimingKeyword::Linear),
        }
    }

    #[test]
    fn active_duration_is_zero_when_duration_or_count_is_zero() {
        let mut timing = linear(0.0);
        timing.iteration_count = f64::INFINITY;
        assert_eq!(active_duration(&timing), 0.0);
        let mut timing = linear(1000.0);
        timing.iteration_count = 0.0;
        assert_eq!(active_duration(&timing), 0.0);
        let mut timing = linear(1000.0);
        timing.iteration_count = 2.5;
        assert_eq!(active_duration(&timing), 2500.0);
    }

    #[test]
    fn end_time_includes_both_delays() {
        let mut timing = linear(1000.0);
        timing.start_delay = 100.0;
        timing.end_delay = 50.0;
        assert_eq!(end_time(&timing), 1150.0);
        timing.end_delay = -2000.0;
        assert_eq!(end_time(&timing), 0.0);
    }

    #[test]
    fn progress_is_linear_in_the_active_phase() {
        let computed = compute(&linear(1000.0), Some(250.0), 1.0);
        assert_eq!(computed.progress, Some(0.25));
        assert_eq!(computed.current_iteration, Some(0.0));
        assert_eq!(computed.local_time, Some(250.0));
    }

    #[test]
    fn unresolved_local_time_is_idle() {
        let computed = compute(&linear(1000.0), None, 1.0);
        assert_eq!(computed.progress, None);
        assert_eq!(computed.current_iteration, None);
        assert_eq!(phase(&linear(1000.0), None, 1.0), Phase::Idle);
    }

    #[test]
    fn before_phase_only_applies_with_a_backwards_fill() {
        let mut timing = linear(1000.0);
        timing.start_delay = 100.0;
        assert_eq!(phase(&timing, Some(50.0), 1.0), Phase::Before);
        assert_eq!(compute(&timing, Some(50.0), 1.0).progress, None);
        timing.fill = Fill::Backwards;
        assert_eq!(compute(&timing, Some(50.0), 1.0).progress, Some(0.0));
    }

    #[test]
    fn after_phase_only_applies_with_a_forwards_fill() {
        let mut timing = linear(1000.0);
        assert_eq!(phase(&timing, Some(1500.0), 1.0), Phase::After);
        assert_eq!(compute(&timing, Some(1500.0), 1.0).progress, None);
        timing.fill = Fill::Forwards;
        let computed = compute(&timing, Some(1500.0), 1.0);
        assert_eq!(computed.progress, Some(1.0));
        assert_eq!(computed.current_iteration, Some(0.0));
    }

    #[test]
    fn the_end_boundary_belongs_to_the_after_phase_when_going_forwards() {
        let timing = linear(1000.0);
        assert_eq!(phase(&timing, Some(1000.0), 1.0), Phase::After);
        assert_eq!(phase(&timing, Some(1000.0), -1.0), Phase::Active);
        assert_eq!(phase(&timing, Some(0.0), 1.0), Phase::Active);
        assert_eq!(phase(&timing, Some(0.0), -1.0), Phase::Before);
    }

    #[test]
    fn reverse_direction_counts_down() {
        let mut timing = linear(1000.0);
        timing.direction = Direction::Reverse;
        assert_eq!(compute(&timing, Some(250.0), 1.0).progress, Some(0.75));
    }

    #[test]
    fn alternate_flips_on_odd_iterations() {
        let mut timing = linear(1000.0);
        timing.iteration_count = 2.0;
        timing.direction = Direction::Alternate;
        let computed = compute(&timing, Some(1250.0), 1.0);
        assert_eq!(computed.current_iteration, Some(1.0));
        assert_eq!(computed.progress, Some(0.75));
        timing.direction = Direction::AlternateReverse;
        assert_eq!(compute(&timing, Some(1250.0), 1.0).progress, Some(0.25));
    }

    #[test]
    fn iteration_start_offsets_the_progress() {
        let mut timing = linear(1000.0);
        timing.iteration_start = 0.5;
        assert_eq!(compute(&timing, Some(0.0), 1.0).progress, Some(0.5));
    }

    #[test]
    fn infinite_iterations_never_finish() {
        let mut timing = linear(1000.0);
        timing.iteration_count = f64::INFINITY;
        assert_eq!(end_time(&timing), f64::INFINITY);
        assert_eq!(phase(&timing, Some(1e9), 1.0), Phase::Active);
        assert_eq!(
            compute(&timing, Some(2500.0), 1.0).current_iteration,
            Some(2.0)
        );
    }

    #[test]
    fn easing_is_applied_to_the_directed_progress() {
        let mut timing = linear(1000.0);
        timing.easing = TimingFunction::ease();
        let progress = compute(&timing, Some(500.0), 1.0).progress.unwrap();
        // cubic-bezier(0.25, 0.1, 0.25, 1) at 0.5 is 0.8024; the solver
        // stops at the epsilon tolerance (0.005 for a one-second effect),
        // so allow that much error in the output.
        assert!((progress - 0.8024).abs() < 5e-3, "got {progress}");
    }
}
