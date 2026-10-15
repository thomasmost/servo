/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use cssparser::Parser;
use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::AnimationEffectBinding::{
    AnimationEffectMethods, ComputedEffectTiming, EffectTiming, FillMode, OptionalEffectTiming,
    PlaybackDirection,
};
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::error::{Error, Fallible};
use script_bindings::num::Finite;
use script_bindings::reflector::Reflector;
use script_bindings::root::Dom;
use script_bindings::str::DOMString;
use style::parser::Parse;
use style::stylesheets::CssRuleType;
use style::values::generics::easing::TimingKeyword;
use style::values::specified::TimingFunction;
use style_traits::{ParsingMode, ToCss};

use crate::css::css::parser_context_for_document;
use crate::dom::Window;
use crate::dom::animation::Animation;
use crate::dom::animations::timing::{self, Timing};
use crate::dom::bindings::codegen::UnionTypes::UnrestrictedDoubleOrString;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};

/// <https://drafts.csswg.org/web-animations-1/#animationeffect>
#[dom_struct]
pub(crate) struct AnimationEffect {
    reflector: Reflector,

    /// The window that this `AnimationEffect` was constructed in.
    window: Dom<Window>,

    specified_timing_properties: DomRefCell<SpecifiedTimingProperties>,

    /// The animation this effect is associated with, if any.
    ///
    /// <https://drafts.csswg.org/web-animations-1/#animation-associated-effect>
    animation: MutNullableDom<Animation>,
}

#[derive(Clone, JSTraceable, MallocSizeOf)]
struct SpecifiedTimingProperties {
    /// <https://drafts.csswg.org/web-animations-1/#start-delay>
    start_delay: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#end-delay>
    end_delay: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#fill-mode>
    fill_mode: FillMode,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-count>
    iteration_count: f64,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-start>
    iteration_start: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-duration>
    iteration_duration: IterationDurationOrAuto,

    /// <https://drafts.csswg.org/web-animations-1/#playback-direction>
    playback_direction: PlaybackDirection,

    /// <https://drafts.csswg.org/css-easing-2/#easing-function>
    #[no_trace]
    easing_function: TimingFunction,
}

impl AnimationEffect {
    pub(crate) fn new_inherited(window: &Window) -> Self {
        Self {
            reflector: Reflector::new(),
            window: Dom::from_ref(window),

            // The defaults of the EffectTiming dictionary: members that the
            // constructor's options do not mention keep these.
            specified_timing_properties: DomRefCell::new(SpecifiedTimingProperties {
                start_delay: Finite::wrap(0.0),
                end_delay: Finite::wrap(0.0),
                fill_mode: FillMode::Auto,
                iteration_count: 1.0,
                iteration_start: Finite::wrap(0.0),
                iteration_duration: IterationDurationOrAuto::Auto,
                playback_direction: PlaybackDirection::Normal,
                easing_function: TimingFunction::Keyword(TimingKeyword::Linear),
            }),
            animation: Default::default(),
        }
    }

    pub(crate) fn window(&self) -> &Window {
        &self.window
    }

    pub(crate) fn animation(&self) -> Option<DomRoot<Animation>> {
        self.animation.get()
    }

    /// Called by `Animation::set_the_associated_effect`; keeps the back
    /// pointer that local time and finished-state updates need.
    pub(crate) fn set_animation(&self, animation: Option<&Animation>) {
        self.animation.set(animation);
    }

    /// The timing properties with `auto` resolved, as the timing model
    /// consumes them.
    pub(crate) fn timing(&self) -> Timing {
        let properties = self.specified_timing_properties.borrow();
        Timing {
            start_delay: *properties.start_delay,
            end_delay: *properties.end_delay,
            fill: match properties.fill_mode {
                FillMode::None | FillMode::Auto => timing::Fill::None,
                FillMode::Forwards => timing::Fill::Forwards,
                FillMode::Backwards => timing::Fill::Backwards,
                FillMode::Both => timing::Fill::Both,
            },
            iteration_start: *properties.iteration_start,
            iteration_count: properties.iteration_count,
            iteration_duration: properties.iteration_duration.computed_value(),
            direction: match properties.playback_direction {
                PlaybackDirection::Normal => timing::Direction::Normal,
                PlaybackDirection::Reverse => timing::Direction::Reverse,
                PlaybackDirection::Alternate => timing::Direction::Alternate,
                PlaybackDirection::Alternate_reverse => timing::Direction::AlternateReverse,
            },
            easing: properties
                .easing_function
                .to_computed_value_without_context(),
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#end-time>
    pub(crate) fn end_time(&self) -> f64 {
        timing::end_time(&self.timing())
    }

    /// <https://drafts.csswg.org/web-animations-1/#update-the-timing-properties-of-an-animation-effect>
    pub(crate) fn update_the_timing_properties(
        &self,
        cx: &mut JSContext,
        input: &OptionalEffectTiming,
    ) -> Fallible<()> {
        // Step 1. If the iterationStart member of input exists and is less than zero,
        // throw a TypeError and abort this procedure.
        if input
            .iterationStart
            .is_some_and(|iteration_start| *iteration_start < 0.0)
        {
            return Err(Error::Type(
                c"Negative values for iterationStart are not allowed".to_owned(),
            ));
        }

        // Step 2. If the iterations member of input exists, and is less than zero or is the value NaN,
        // throw a TypeError and abort this procedure.
        if input
            .iterations
            .is_some_and(|iterations| iterations < 0.0 || iterations.is_nan())
        {
            return Err(Error::Type(
                c"\"iterations\" must be a positive number".to_owned(),
            ));
        }

        // Step 3. If the duration member of input exists, and is less than zero, is the value NaN,
        // or is a string other than the lowercase auto, throw a TypeError and abort this procedure.
        let Ok(duration) = input
            .duration
            .as_ref()
            .map(|duration| match duration {
                UnrestrictedDoubleOrString::UnrestrictedDouble(double) => {
                    if *double < 0.0 || double.is_nan() {
                        Err(())
                    } else {
                        Ok(IterationDurationOrAuto::Duration(*double))
                    }
                },
                UnrestrictedDoubleOrString::String(string) => {
                    if string == "auto" {
                        Ok(IterationDurationOrAuto::Auto)
                    } else {
                        Err(())
                    }
                },
            })
            .transpose()
        else {
            return Err(Error::Type(
                c"\"duration\" must be a positive number".to_owned(),
            ));
        };

        // Step 4. If the easing member of input exists but cannot be parsed using the <easing-function> production [CSS-EASING-1],
        // throw a TypeError and abort this procedure.
        let Ok(easing) = input
            .easing
            .as_ref()
            .map(|easing| {
                let easing = easing.str();
                let mut parser = Parser::new(&easing);

                // None of these values should matter
                let document = self.window.Document();
                let urlextradata = document.url().into_url().into();
                let parser_context = parser_context_for_document(
                    &document,
                    CssRuleType::Style,
                    ParsingMode::DEFAULT,
                    &urlextradata,
                );
                TimingFunction::parse(&parser_context, &mut parser).map_err(|_| ())
            })
            .transpose()
        else {
            return Err(Error::Type(
                c"\"easing\" is not a valid timing function".to_owned(),
            ));
        };

        // Step 5. Assign each member that exists in input to the corresponding timing property of effect as follows:
        // delay → start delay
        let mut specified_timing_properties = self.specified_timing_properties.borrow_mut();
        if let Some(start_delay) = input.delay {
            specified_timing_properties.start_delay = start_delay;
        }

        // endDelay → end delay
        if let Some(end_delay) = input.endDelay {
            specified_timing_properties.end_delay = end_delay;
        }

        // fill → fill mode
        if let Some(fill) = input.fill {
            specified_timing_properties.fill_mode = fill;
        }

        // iterationStart → iteration start
        if let Some(iteration_start) = input.iterationStart {
            specified_timing_properties.iteration_start = iteration_start;
        }

        // iterations → iteration count
        if let Some(iterations) = input.iterations {
            specified_timing_properties.iteration_count = iterations;
        }

        // duration → iteration duration
        if let Some(duration) = duration {
            specified_timing_properties.iteration_duration = duration;
        }

        // direction → playback direction
        if let Some(direction) = input.direction {
            specified_timing_properties.playback_direction = direction;
        }

        // easing → easing function
        if let Some(easing) = easing {
            specified_timing_properties.easing_function = easing;
        }
        drop(specified_timing_properties);

        // Step 6. If effect is associated with an animation, run the
        // procedure to update an animation's finished state.
        if let Some(animation) = self.animation.get() {
            animation.update_finished_state(cx, false, false);
        }
        Ok(())
    }
}

impl AnimationEffectMethods<crate::DomTypeHolder> for AnimationEffect {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-gettiming>
    fn GetTiming(&self) -> EffectTiming {
        // > Returns the specified timing properties for this animation effect.
        let specified_timing_properties = self.specified_timing_properties.borrow();
        EffectTiming {
            delay: specified_timing_properties.start_delay,
            direction: specified_timing_properties.playback_direction,
            duration: specified_timing_properties.iteration_duration.into(),
            easing: specified_timing_properties
                .easing_function
                .to_css_string()
                .into(),
            endDelay: specified_timing_properties.end_delay,
            fill: specified_timing_properties.fill_mode,
            iterationStart: specified_timing_properties.iteration_start,
            iterations: specified_timing_properties.iteration_count,
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-getcomputedtiming>
    fn GetComputedTiming(&self) -> ComputedEffectTiming {
        // > Returns the calculated timing properties for this animation effect.
        let specified_timing_properties = self.specified_timing_properties.borrow();

        let (local_time, playback_rate) = match self.animation.get() {
            Some(animation) => (animation.current_time(), animation.playback_rate()),
            None => (None, 1.0),
        };
        let computed = timing::compute(&self.timing(), local_time, playback_rate);

        // > while getTiming() can return the string auto, getComputedTiming() must return a number
        // > corresponding to the calculated value of the iteration duration as defined in the description
        // > of the duration member of the EffectTiming interface.
        // > In this level of the specification, that simply means that an auto value is replaced by zero.
        let computed_duration = specified_timing_properties
            .iteration_duration
            .computed_value();

        // > likewise, while getTiming() can return the string auto, getComputedTiming() must return the
        // > specific FillMode used for timing calculations as defined in the description of the fill
        // > member of the EffectTiming interface.
        // > In this level of the specification, that simply means that an auto value is replaced by the none FillMode.
        let computed_fill_mode = if specified_timing_properties.fill_mode == FillMode::Auto {
            FillMode::None
        } else {
            specified_timing_properties.fill_mode
        };

        ComputedEffectTiming {
            parent: EffectTiming {
                delay: specified_timing_properties.start_delay,
                direction: specified_timing_properties.playback_direction,
                duration: UnrestrictedDoubleOrString::UnrestrictedDouble(computed_duration),
                easing: specified_timing_properties
                    .easing_function
                    .to_css_string()
                    .into(),
                endDelay: specified_timing_properties.end_delay,
                fill: computed_fill_mode,
                iterationStart: specified_timing_properties.iteration_start,
                iterations: specified_timing_properties.iteration_count,
            },
            endTime: Some(computed.end_time),
            activeDuration: Some(computed.active_duration),
            localTime: Some(computed.local_time.and_then(Finite::new)),
            progress: Some(computed.progress.and_then(Finite::new)),
            currentIteration: Some(computed.current_iteration),
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-updatetiming>
    fn UpdateTiming(&self, cx: &mut JSContext, timing: &OptionalEffectTiming) -> Fallible<()> {
        // > Updates the specified timing properties of this animation effect by performing the procedure
        // > to update the timing properties of an animation effect passing the timing parameter as input.
        self.update_the_timing_properties(cx, timing)
    }
}

/// Every member of `timing` as an `OptionalEffectTiming`: the "timing
/// input" of the KeyframeEffect constructor for a dictionary argument.
pub(crate) fn optional_effect_timing(timing: &EffectTiming) -> OptionalEffectTiming {
    OptionalEffectTiming {
        delay: Some(timing.delay),
        direction: Some(timing.direction),
        duration: Some(match &timing.duration {
            UnrestrictedDoubleOrString::UnrestrictedDouble(duration) => {
                UnrestrictedDoubleOrString::UnrestrictedDouble(*duration)
            },
            UnrestrictedDoubleOrString::String(duration) => {
                UnrestrictedDoubleOrString::String(duration.clone())
            },
        }),
        easing: Some(timing.easing.clone()),
        endDelay: Some(timing.endDelay),
        fill: Some(timing.fill),
        iterationStart: Some(timing.iterationStart),
        iterations: Some(timing.iterations),
    }
}

/// The "timing input" for a bare number argument: a duration, with every
/// other member left at its default.
pub(crate) fn optional_effect_timing_for_duration(duration: f64) -> OptionalEffectTiming {
    OptionalEffectTiming {
        delay: None,
        direction: None,
        duration: Some(UnrestrictedDoubleOrString::UnrestrictedDouble(duration)),
        easing: None,
        endDelay: None,
        fill: None,
        iterationStart: None,
        iterations: None,
    }
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf)]
enum IterationDurationOrAuto {
    Duration(f64),
    Auto,
}

impl IterationDurationOrAuto {
    fn computed_value(&self) -> f64 {
        match self {
            IterationDurationOrAuto::Auto => 0.0,
            IterationDurationOrAuto::Duration(double) => *double,
        }
    }
}

impl From<IterationDurationOrAuto> for UnrestrictedDoubleOrString {
    fn from(value: IterationDurationOrAuto) -> Self {
        match value {
            IterationDurationOrAuto::Auto => {
                UnrestrictedDoubleOrString::String(DOMString::from_static("auto"))
            },
            IterationDurationOrAuto::Duration(double) => {
                UnrestrictedDoubleOrString::UnrestrictedDouble(double)
            },
        }
    }
}
