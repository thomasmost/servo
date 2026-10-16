/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::cmp::Ordering;
use std::ffi::{CStr, CString};
use std::ops::ControlFlow;
use std::ptr::{self, NonNull};
use std::sync::LazyLock;

use cssparser::Parser;
use dom_struct::dom_struct;
use js::context::JSContext;
use js::conversions::{
    ConversionResult, FromJSValConvertible, ToJSValConvertible, jsstr_to_string,
};
use js::gc::{HandleValue, RootedVec};
use js::jsapi::{HandleId, Heap, JS_GetProperty, JSITER_OWNONLY, JSObject, JSPROP_ENUMERATE};
use js::jsval::{ObjectValue, UndefinedValue};
use js::rust::wrappers2::{GetPropertyKeys, JS_DefineProperty, JS_IdToValue, JS_NewObject};
use js::rust::{
    ForOfIterationFailure, HandleObject, IdVector, IntoHandle, IntoMutableHandle, for_of,
};
use rustc_hash::FxHashMap;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::KeyframeEffectBinding::{
    BaseKeyframe, CompositeOperation, CompositeOperationOrAuto,
};
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::codegen::GenericUnionTypes::UnrestrictedDoubleOrKeyframeEffectOptions;
use script_bindings::conversions::StringificationBehavior;
use script_bindings::error::{Error, Fallible};
use script_bindings::inheritance::Castable;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::root::DomRoot;
use script_bindings::str::DOMString;
use servo_arc::Arc as ServoArc;
use style::animation::{ScriptAnimation, ScriptAnimationId, ScriptKeyframe};
use style::parser::{Parse, ParserContext};
use style::properties::generated::PropertyDeclaration;
use style::properties::{
    Importance, LonghandId, NonCustomPropertyId, PropertyDeclarationBlock, PropertyId,
    SourcePropertyDeclaration,
};
use style::stylesheets::CssRuleType;
use style::values::computed::AnimationComposition;
use style::values::specified::TimingFunction;
use style_traits::{CssWriter, ParsingMode, ToCss};

use crate::css::css::parser_context_for_document;
use crate::dom::Document;
use crate::dom::animationeffect::AnimationEffect;
use crate::dom::animations::animationeffect::{
    optional_effect_timing, optional_effect_timing_for_duration,
};
use crate::dom::bindings::codegen::Bindings::KeyframeEffectBinding::{
    BaseComputedKeyframe, KeyframeEffectMethods,
};
use crate::dom::bindings::root::MutNullableDom;
use crate::dom::element::Element;
use crate::dom::window::Window;

/// <https://drafts.csswg.org/web-animations-1/#keyframeeffect>
#[dom_struct]
pub(crate) struct KeyframeEffect {
    animationeffect: AnimationEffect,

    /// <https://drafts.csswg.org/web-animations-1/#effect-target-target-element>
    ///
    /// Pseudo-element targets are not supported: the target pseudo-selector
    /// is always null.
    target_element: MutNullableDom<Element>,

    /// <https://drafts.csswg.org/web-animations-1/#effect-composite-operation>
    composite: Cell<CompositeOperation>,

    /// <https://drafts.csswg.org/web-animations-1/#keyframe>
    keyframes: DomRefCell<Vec<Keyframe>>,

    /// Bumped whenever the keyframes, composite or target change, so the
    /// animation re-pushes the effect into stylo.
    generation: Cell<u64>,
}

impl KeyframeEffect {
    pub(crate) fn new_inherited(window: &Window) -> Self {
        Self {
            animationeffect: AnimationEffect::new_inherited(window),
            target_element: Default::default(),
            composite: Cell::new(CompositeOperation::Replace),
            keyframes: Default::default(),
            generation: Cell::new(0),
        }
    }

    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        reflect_dom_object_with_proto(cx, Box::new(Self::new_inherited(window)), window, proto)
    }

    /// The effect target, when it is an element.
    pub(crate) fn target(&self) -> Option<DomRoot<Element>> {
        self.target_element.get()
    }

    /// Changes when the keyframes, composite or target change.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.get()
    }

    fn bump_generation(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
    }

    /// This effect's keyframes in the form stylo samples: computed offsets,
    /// computed easing, per-keyframe composite, and one declaration block
    /// per keyframe locked by the document's author lock (the style
    /// traversal reads them with its author guard).
    pub(crate) fn to_script_animation(
        &self,
        id: ScriptAnimationId,
        document: &Document,
        iteration_duration_seconds: f64,
    ) -> ScriptAnimation {
        let keyframes = self.keyframes.borrow();
        let offsets = compute_missing_keyframe_offsets(
            &keyframes
                .iter()
                .map(|keyframe| keyframe.offset)
                .collect::<Vec<_>>(),
        );
        let lock = &document.shared_style_locks().author;
        let script_keyframes = keyframes
            .iter()
            .zip(offsets)
            .map(|(keyframe, offset)| {
                let mut block = PropertyDeclarationBlock::new();
                for declaration in &keyframe.declarations {
                    for (property, importance) in declaration.block.declaration_importance_iter() {
                        block.push(property.clone(), importance);
                    }
                }
                ScriptKeyframe {
                    offset,
                    easing: keyframe.easing.to_computed_value_without_context(),
                    composite: composite_or_auto_to_stylo(keyframe.composite),
                    declarations: ServoArc::new(lock.wrap(block)),
                }
            })
            .collect();
        ScriptAnimation::new(
            id,
            script_keyframes,
            composite_to_stylo(self.composite.get()),
            iteration_duration_seconds,
        )
    }
}

fn composite_to_stylo(composite: CompositeOperation) -> AnimationComposition {
    match composite {
        CompositeOperation::Replace => AnimationComposition::Replace,
        CompositeOperation::Add => AnimationComposition::Add,
        CompositeOperation::Accumulate => AnimationComposition::Accumulate,
    }
}

fn composite_or_auto_to_stylo(composite: CompositeOperationOrAuto) -> Option<AnimationComposition> {
    match composite {
        CompositeOperationOrAuto::Auto => None,
        CompositeOperationOrAuto::Replace => Some(AnimationComposition::Replace),
        CompositeOperationOrAuto::Add => Some(AnimationComposition::Add),
        CompositeOperationOrAuto::Accumulate => Some(AnimationComposition::Accumulate),
    }
}

impl KeyframeEffectMethods<crate::DomTypeHolder> for KeyframeEffect {
    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-keyframeeffect>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        target: Option<&Element>,
        keyframes: *mut JSObject,
        options: UnrestrictedDoubleOrKeyframeEffectOptions,
    ) -> Fallible<DomRoot<KeyframeEffect>> {
        // Step 1. Create a new KeyframeEffect object, effect.
        let effect = KeyframeEffect::new_with_proto(cx, window, proto);

        // Step 2. Set the target element of effect to target.
        effect.target_element.set(target);

        // Step 3. The target pseudo-selector is always null: pseudo-element
        // targets are not supported.

        // Step 4. Let timing input be the result corresponding to the first
        // matching condition below: a KeyframeEffectOptions object is used as
        // is; a number is a duration with every other member at its default.
        let (timing_input, composite) = match options {
            UnrestrictedDoubleOrKeyframeEffectOptions::UnrestrictedDouble(duration) => {
                (optional_effect_timing_for_duration(duration), None)
            },
            UnrestrictedDoubleOrKeyframeEffectOptions::KeyframeEffectOptions(options) => (
                optional_effect_timing(&options.parent),
                Some(options.composite),
            ),
        };

        // Step 5. Call the procedure to update the timing properties of an
        // animation effect of effect from timing input. If that procedure
        // causes an exception to be thrown, propagate the exception and abort
        // this procedure.
        effect
            .upcast::<AnimationEffect>()
            .update_the_timing_properties(cx, &timing_input)?;

        // Step 6. If options is a KeyframeEffectOptions object, assign the
        // composite property of effect to the corresponding value from options.
        if let Some(composite) = composite {
            effect.composite.set(composite);
        }

        // Step 7. Initialize the set of keyframes by performing the procedure
        // defined for setKeyframes() passing keyframes as the input.
        effect.SetKeyframes(cx, keyframes)?;

        Ok(effect)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-target>
    fn GetTarget(&self) -> Option<DomRoot<Element>> {
        self.target_element.get()
    }

    fn SetTarget(&self, target: Option<&Element>) {
        self.target_element.set(target);
        self.bump_generation();
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-composite>
    fn Composite(&self) -> CompositeOperation {
        self.composite.get()
    }

    fn SetComposite(&self, composite: CompositeOperation) {
        self.composite.set(composite);
        self.bump_generation();
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-getkeyframes>
    #[expect(unsafe_code)]
    fn GetKeyframes(
        &self,
        cx: &mut JSContext,
        result: &mut RootedVec<'_, Box<Heap<*mut JSObject>>>,
    ) -> Fallible<()> {
        let mut layout = self.upcast::<AnimationEffect>().window().layout_mut();
        let stylist = layout.stylist_mut();

        // Step 1. Let result be an empty sequence of objects.
        debug_assert!(result.is_empty());

        // Step 2. Let keyframes be the result of applying the procedure
        // compute missing keyframe offsets to the keyframes for this keyframe
        // effect. (There are no CSSAnimation objects here.)
        let keyframes = self.keyframes.borrow();
        let computed_offsets = compute_missing_keyframe_offsets(
            &keyframes
                .iter()
                .map(|keyframe| keyframe.offset)
                .collect::<Vec<_>>(),
        );

        // Step 3. For each keyframe in keyframes perform the following steps:
        for (keyframe, computed_offset) in keyframes.iter().zip(computed_offsets) {
            // Step 3.1 Initialize a dictionary object, output keyframe.
            // Step 3.2 Set the offset, computedOffset, easing, and composite
            // members of output keyframe to the respective keyframe offset,
            // computed keyframe offset, keyframe-specific easing function, and
            // keyframe-specific composite operation values of keyframe.
            let base_keyframe = BaseComputedKeyframe {
                composite: keyframe.composite,
                offset: keyframe.offset.and_then(Finite::new),
                // `computedOffset` has no default in the IDL, so the
                // generated field is an Option.
                computedOffset: Some(Finite::wrap(computed_offset)),
                easing: keyframe.easing.to_css_string().into(),
            };
            rooted!(&in(cx) let mut output_keyframe = unsafe { JS_NewObject(cx, ptr::null()) });
            base_keyframe.to_jsobject(cx, output_keyframe.handle_mut());

            // Step 3.3 For each animation property-value pair declaration in
            // keyframe, perform the following steps:
            for property_value_pair in &keyframe.declarations {
                debug_assert!(property_value_pair.property_id.is_animatable());

                // Step 3.3.1 Let property name be the result of applying the
                // animation property name to IDL attribute name algorithm to the
                // property name of declaration.
                let mut property_name = String::new();
                let mut writer = CssWriter::new(&mut property_name);
                if property_value_pair.property_id.to_css(&mut writer).is_err() {
                    continue;
                }
                let property_name = animation_property_name_to_idl_attribute_name(&property_name);

                // Step 3.3.2 Let IDL value be the result of serializing the
                // property value of declaration by passing declaration to the
                // algorithm to serialize a CSS value [CSSOM].
                let mut value_string = String::new();
                if property_value_pair
                    .block
                    .single_value_to_css(
                        &property_value_pair.property_id,
                        &mut value_string,
                        None,
                        stylist,
                    )
                    .is_err()
                {
                    continue;
                }

                // Step 3.3.3 Let value be the result of converting IDL value to
                // an ECMAScript String value.
                rooted!(&in(cx) let mut value = UndefinedValue());
                value_string.to_jsval(cx, value.handle_mut());

                // Step 3.3.4 Call the [[DefineOwnProperty]] internal method on
                // output keyframe with property name property name, Property
                // Descriptor { [[Writable]]: true, [[Enumerable]]: true,
                // [[Configurable]]: true, [[Value]]: value } and Boolean flag
                // false.
                let Ok(property_name) = CString::new(property_name) else {
                    continue;
                };

                let success = unsafe {
                    JS_DefineProperty(
                        cx,
                        output_keyframe.handle(),
                        property_name.as_ptr(),
                        value.handle(),
                        JSPROP_ENUMERATE as u32,
                    )
                };
                if !success {
                    if cfg!(debug_assertions) {
                        unreachable!("Setting a property on output_keyframe should never fail");
                    }
                    return Err(Error::Operation(None));
                }
            }

            // Step 3.4 Append output keyframe to result.
            result.push(Heap::boxed(output_keyframe.get()))
        }

        // Step 4. Return result.
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-setkeyframes>
    fn SetKeyframes(&self, cx: &mut JSContext, keyframes: *mut JSObject) -> Fallible<()> {
        // > This effect’s set of keyframes is replaced with the result of
        // > performing the procedure to process a keyframes argument. If that
        // > procedure throws an exception, this effect’s keyframes are not
        // > modified.
        let document = self.upcast::<AnimationEffect>().window().Document();
        let keyframes = process_a_keyframes_argument(cx, &document, keyframes)?;
        *self.keyframes.safe_borrow_mut(cx.no_gc()) = keyframes;
        self.bump_generation();
        Ok(())
    }
}

/// <https://drafts.csswg.org/web-animations-1/#keyframe>
#[derive(JSTraceable, MallocSizeOf)]
struct Keyframe {
    /// <https://drafts.csswg.org/web-animations-1/#keyframe-offset>
    offset: Option<f64>,
    /// <https://drafts.csswg.org/web-animations-1/#keyframe-specific-easing-function>
    #[no_trace]
    easing: TimingFunction,
    /// <https://drafts.csswg.org/web-animations-1/#keyframe-specific-composite-operation>
    composite: CompositeOperationOrAuto,
    declarations: Vec<KeyframePropertyDeclaration>,
}

/// A keyframe whose easing has been read but not yet parsed: the spec
/// parses easings only after every property of the input was read.
struct RawKeyframe {
    offset: Option<f64>,
    easing: DOMString,
    composite: CompositeOperationOrAuto,
    declarations: Vec<KeyframePropertyDeclaration>,
}

#[derive(JSTraceable, MallocSizeOf)]
struct KeyframePropertyDeclaration {
    #[no_trace]
    property_id: PropertyId,
    /// The block is known to only contain declarations for `property_id`. There might
    /// be more than one value if `property_id` is a shorthand.
    #[no_trace]
    block: PropertyDeclarationBlock,
}

fn type_error(message: &'static CStr) -> Error {
    Error::Type(message.to_owned())
}

/// <https://drafts.csswg.org/web-animations-1/#process-a-keyframes-argument>
fn process_a_keyframes_argument(
    cx: &mut JSContext,
    document: &Document,
    keyframes: *mut JSObject,
) -> Fallible<Vec<Keyframe>> {
    // Step 1. If object is null, return an empty sequence of keyframes.
    if keyframes.is_null() {
        return Ok(Vec::new());
    }

    // Step 2. Let processed keyframes be an empty sequence of keyframes.
    let mut processed_keyframes: Vec<RawKeyframe> = Vec::new();
    let mut unused_easings: Vec<DOMString> = Vec::new();

    // Step 3. Let method be the result of GetMethod(object, @@iterator).
    // Step 4. Check the completion record of method.
    // Step 5. Perform the steps corresponding to the first matching condition
    // below. `for_of` reports a missing @@iterator as ValueIsNotIterable.
    rooted!(&in(cx) let iterable = ObjectValue(keyframes));
    let result = for_of(cx, iterable.handle(), |cx, iterator_element| {
        // Step 5.3.4 Let nextItem be IteratorValue(next).
        // Step 5.3.5 Check the completion record of nextItem.
        // Note: This happens inside the "for_of" call.

        // Step 5.3.6 If Type(nextItem) is not Undefined, Null or Object, then
        // throw a TypeError and abort these steps.
        if !iterator_element.is_null_or_undefined() && !iterator_element.is_object() {
            return Err(ForOfIterationFailure::Other(type_error(
                c"Keyframe must be an object, null or undefined",
            )));
        }

        // Step 5.3.7 Append to processed keyframes the result of running the
        // procedure to process a keyframe-like object passing nextItem as the
        // keyframe input with the allow lists flag set to false.
        processed_keyframes.push(process_a_keyframe_like_object(
            cx,
            document,
            iterator_element,
        )?);

        Ok(ControlFlow::Continue(()))
    });
    match result {
        Ok(()) => {},
        Err(ForOfIterationFailure::ValueIsNotIterable) => {
            // `iterable` is rooted and follows any move by the GC while the
            // iterator ran script, so the object is taken from it.
            rooted!(&in(cx) let object = iterable.handle().to_object());
            unused_easings = process_a_property_indexed_keyframes_argument(
                cx,
                document,
                object.handle(),
                &mut processed_keyframes,
            )?;
        },
        Err(ForOfIterationFailure::JSFailed) => return Err(Error::JSFailed),
        Err(ForOfIterationFailure::Other(error)) => return Err(error),
    }

    // Step 6. If processed keyframes is not loosely sorted by offset, throw a
    // TypeError and abort these steps.
    let mut previous_offset: Option<f64> = None;
    for keyframe in &processed_keyframes {
        let Some(offset) = keyframe.offset else {
            continue;
        };
        if previous_offset.is_some_and(|previous| offset < previous) {
            return Err(type_error(c"Keyframes are not loosely sorted by offset"));
        }
        previous_offset = Some(offset);
    }

    // Step 7. If there exist any keyframe in processed keyframes whose keyframe
    // offset is non-null and less than zero or greater than one, throw a
    // TypeError and abort these steps.
    if processed_keyframes.iter().any(|keyframe| {
        keyframe
            .offset
            .is_some_and(|offset| !(0.0..=1.0).contains(&offset))
    }) {
        return Err(type_error(c"Keyframe offsets must be between 0 and 1"));
    }

    // Step 8. For each frame in processed keyframes, perform the following steps:
    // Step 8.1 happened while reading: invalid property values were discarded.
    // Step 8.2 Let the easing function of frame be the result of parsing the
    // "easing" property on frame using the CSS syntax defined for the easing
    // member of the EffectTiming dictionary. If parsing the "easing" property
    // fails, throw a TypeError and abort this procedure.
    let urlextradata = document.url().into_url().into();
    let parser_context = parser_context_for_document(
        document,
        CssRuleType::Style,
        ParsingMode::DEFAULT,
        &urlextradata,
    );
    let mut keyframes = Vec::with_capacity(processed_keyframes.len());
    for keyframe in processed_keyframes {
        let easing = parse_easing(&keyframe.easing, &parser_context)
            .ok_or_else(|| type_error(c"Keyframe easing is not a valid timing function"))?;
        keyframes.push(Keyframe {
            offset: keyframe.offset,
            easing,
            composite: keyframe.composite,
            declarations: keyframe.declarations,
        });
    }

    // Step 9. Parse each of the values in unused easings using the CSS syntax
    // defined for easing member of the EffectTiming interface, and if any of
    // the values fail to parse, throw a TypeError and abort this procedure.
    for easing in unused_easings {
        parse_easing(&easing, &parser_context)
            .ok_or_else(|| type_error(c"Keyframe easing is not a valid timing function"))?;
    }

    Ok(keyframes)
}

/// Parses `input` with the `<easing-function>` production, whole.
fn parse_easing(input: &DOMString, parser_context: &ParserContext<'_>) -> Option<TimingFunction> {
    let input = input.str();
    let mut parser = Parser::new(&input);
    parser
        .parse_entirely(|parser| TimingFunction::parse(parser_context, parser))
        .ok()
}

/// <https://drafts.csswg.org/web-animations-1/#process-a-keyframe-like-object>
/// with the allow lists flag false (the array form).
fn process_a_keyframe_like_object(
    cx: &mut JSContext,
    document: &Document,
    keyframe_input: HandleValue,
) -> Fallible<RawKeyframe> {
    // Step 1. Run the procedure to convert an ECMAScript value to a dictionary
    // type [WEBIDL] with keyframe input as the ECMAScript value, and the
    // dictionary type BaseKeyframe. Store the result of this procedure as
    // keyframe output.
    let Ok(keyframe_output) = BaseKeyframe::from_jsval(cx, keyframe_input, ()) else {
        return Err(Error::JSFailed);
    };
    let ConversionResult::Success(keyframe_output) = keyframe_output else {
        return Err(type_error(
            c"Keyframe has an invalid offset, easing or composite",
        ));
    };
    let mut keyframe_output = RawKeyframe {
        offset: keyframe_output.offset.map(|offset| *offset),
        easing: keyframe_output.easing,
        composite: keyframe_output.composite,
        declarations: Vec::new(),
    };

    // Step 2. If keyframe input is null or undefined, return keyframe output.
    if keyframe_input.is_null_or_undefined() {
        return Ok(keyframe_output);
    }

    // Steps 3 to 7: read the animatable properties of the object.
    let urlextradata = document.url().into_url().into();
    let parser_context = parser_context_for_document(
        document,
        CssRuleType::Style,
        ParsingMode::DEFAULT,
        &urlextradata,
    );
    rooted!(&in(cx) let object = keyframe_input.to_object());
    keyframe_output.declarations =
        get_property_declarations(cx, object.handle(), &parser_context, false)?
            .into_iter()
            .flat_map(|(_, declarations)| declarations)
            .flatten()
            .collect();

    // Step 8. Return keyframe output.
    Ok(keyframe_output)
}

/// The "Otherwise" branch of step 5 of
/// <https://drafts.csswg.org/web-animations-1/#process-a-keyframes-argument>:
/// the property-indexed form. Fills `processed_keyframes` and returns the
/// unused easings for step 9.
fn process_a_property_indexed_keyframes_argument(
    cx: &mut JSContext,
    document: &Document,
    object: HandleObject,
    processed_keyframes: &mut Vec<RawKeyframe>,
) -> Fallible<Vec<DOMString>> {
    let urlextradata = document.url().into_url().into();
    let parser_context = parser_context_for_document(
        document,
        CssRuleType::Style,
        ParsingMode::DEFAULT,
        &urlextradata,
    );

    // Step 5.1 Let property-indexed keyframe be the result of running the
    // procedure to process a keyframe-like object passing object as the
    // keyframe input and with the allow lists flag set to true. Its first step
    // converts the BasePropertyIndexedKeyframe members in dictionary order:
    // composite, easing, then offset. Their values are applied below, but
    // their conversion, and any exception it raises, comes before the
    // animation properties are read.
    let composites = read_member_list(cx, object, c"composite", convert_composite)?;
    let mut easings = read_member_list(cx, object, c"easing", convert_domstring)?;
    let offsets = read_member_list(cx, object, c"offset", convert_nullable_double)?;

    // The animation properties are read after the members above.
    let properties = get_property_declarations(cx, object, &parser_context, true)?;

    // Step 5.2 For each member, m, in property-indexed keyframe (the
    // "composite", "easing" and "offset" members are applied below):
    struct PendingKeyframe {
        computed_offset: f64,
        declarations: Vec<KeyframePropertyDeclaration>,
    }
    let mut pending: Vec<PendingKeyframe> = Vec::new();
    for (_, values) in properties {
        if values.is_empty() {
            continue;
        }
        // Steps 5.2.4 to 5.2.6: one keyframe per value, with a null offset,
        // then compute missing keyframe offsets on that list.
        let offsets = compute_missing_keyframe_offsets(&vec![None; values.len()]);
        for (declaration, computed_offset) in values.into_iter().zip(offsets) {
            pending.push(PendingKeyframe {
                computed_offset,
                declarations: declaration.into_iter().collect(),
            });
        }
    }

    // Step 5.3 Sort processed keyframes by the computed keyframe offset of each
    // keyframe in increasing order.
    pending.sort_by(|a, b| {
        a.computed_offset
            .partial_cmp(&b.computed_offset)
            .unwrap_or(Ordering::Equal)
    });

    // Step 5.4 Merge adjacent keyframes in processed keyframes when they have
    // equal computed keyframe offsets.
    let mut merged: Vec<PendingKeyframe> = Vec::new();
    for keyframe in pending {
        match merged.last_mut() {
            Some(last) if last.computed_offset == keyframe.computed_offset => {
                last.declarations.extend(keyframe.declarations);
            },
            _ => merged.push(keyframe),
        }
    }
    for keyframe in merged {
        processed_keyframes.push(RawKeyframe {
            offset: None,
            easing: DOMString::from("linear"),
            composite: CompositeOperationOrAuto::Auto,
            declarations: keyframe.declarations,
        });
    }

    // Steps 5.5 and 5.6 Assign each value in offsets to the keyframe offset of
    // the keyframe with the corresponding position in processed keyframes until
    // the end of either sequence is reached.
    for (keyframe, offset) in processed_keyframes.iter_mut().zip(offsets) {
        keyframe.offset = offset;
    }

    // Steps 5.7 to 5.11 Easings: default to "linear", repeat to the length of
    // processed keyframes, keep the excess for step 9.
    if easings.is_empty() {
        easings.push(DOMString::from("linear"));
    }
    let unused_easings = if easings.len() > processed_keyframes.len() {
        easings.split_off(processed_keyframes.len())
    } else {
        Vec::new()
    };
    for (index, keyframe) in processed_keyframes.iter_mut().enumerate() {
        keyframe.easing = easings[index % easings.len()].clone();
    }

    // Step 5.12 If the composite member of property-indexed keyframe is not an
    // empty sequence, repeat it likewise and assign every value that is not
    // "auto".
    if !composites.is_empty() {
        for (index, keyframe) in processed_keyframes.iter_mut().enumerate() {
            let composite = composites[index % composites.len()];
            if composite != CompositeOperationOrAuto::Auto {
                keyframe.composite = composite;
            }
        }
    }

    Ok(unused_easings)
}

/// Reads `name` from `object` as `(T or sequence<T>)`: a missing member is
/// an empty list, an iterable object is converted element by element, and
/// anything else is a single value.
#[expect(unsafe_code)]
fn read_member_list<T>(
    cx: &mut JSContext,
    object: HandleObject,
    name: &CStr,
    convert: fn(&mut JSContext, HandleValue) -> Fallible<T>,
) -> Fallible<Vec<T>> {
    rooted!(&in(cx) let mut value = UndefinedValue());
    if !unsafe {
        JS_GetProperty(
            cx.raw_cx(),
            object.into_handle(),
            name.as_ptr(),
            value.handle_mut().into_handle_mut(),
        )
    } {
        return Err(Error::JSFailed);
    }
    if value.is_undefined() {
        return Ok(Vec::new());
    }
    convert_single_or_list(cx, value.handle(), true, convert)
}

/// Converts `value` to a list of `T`: an iterable object is walked when
/// `allow_lists` is set; otherwise the value converts to one element.
fn convert_single_or_list<T>(
    cx: &mut JSContext,
    value: HandleValue,
    allow_lists: bool,
    convert: fn(&mut JSContext, HandleValue) -> Fallible<T>,
) -> Fallible<Vec<T>> {
    // Strings are iterable in JavaScript but convert as a single DOMString;
    // only objects may be sequences.
    if allow_lists && value.is_object() {
        let mut items = Vec::new();
        let result = for_of(cx, value, |cx, item| {
            items.push(convert(cx, item).map_err(ForOfIterationFailure::Other)?);
            Ok(ControlFlow::Continue(()))
        });
        match result {
            Ok(()) => return Ok(items),
            Err(ForOfIterationFailure::ValueIsNotIterable) => {},
            Err(ForOfIterationFailure::JSFailed) => return Err(Error::JSFailed),
            Err(ForOfIterationFailure::Other(error)) => return Err(error),
        }
    }
    Ok(vec![convert(cx, value)?])
}

/// WebIDL `double?`: null and undefined are null, NaN and infinities throw.
fn convert_nullable_double(cx: &mut JSContext, value: HandleValue) -> Fallible<Option<f64>> {
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    match f64::from_jsval(cx, value, ()) {
        Ok(ConversionResult::Success(double)) if double.is_finite() => Ok(Some(double)),
        Ok(_) => Err(type_error(
            c"Keyframe offset must be a finite number or null",
        )),
        Err(_) => Err(Error::JSFailed),
    }
}

/// WebIDL `DOMString`.
fn convert_domstring(cx: &mut JSContext, value: HandleValue) -> Fallible<DOMString> {
    match DOMString::from_jsval(cx, value, StringificationBehavior::Default) {
        Ok(ConversionResult::Success(string)) => Ok(string),
        Ok(ConversionResult::Failure(_)) => {
            Err(type_error(c"Value cannot be converted to a string"))
        },
        Err(_) => Err(Error::JSFailed),
    }
}

/// WebIDL `CompositeOperationOrAuto`.
fn convert_composite(cx: &mut JSContext, value: HandleValue) -> Fallible<CompositeOperationOrAuto> {
    match CompositeOperationOrAuto::from_jsval(cx, value, ()) {
        Ok(ConversionResult::Success(composite)) => Ok(composite),
        Ok(ConversionResult::Failure(_)) => Err(type_error(
            c"composite must be \"replace\", \"add\", \"accumulate\" or \"auto\"",
        )),
        Err(_) => Err(Error::JSFailed),
    }
}

/// Steps 3 to 7 of
/// <https://drafts.csswg.org/web-animations-1/#process-a-keyframe-like-object>.
///
/// Returns, per animatable property found on the object in codepoint order of
/// its IDL attribute name, the parsed declaration of each value; `None` marks a
/// value that did not parse (discarded per step 8.1 of "process a keyframes
/// argument", but still counted as a keyframe in the property-indexed form).
#[expect(unsafe_code)]
fn get_property_declarations(
    cx: &mut JSContext,
    object: HandleObject,
    parser_context: &ParserContext<'_>,
    allow_lists: bool,
) -> Fallible<
    Vec<(
        NonCustomPropertyId,
        Vec<Option<KeyframePropertyDeclaration>>,
    )>,
> {
    // The spec tells us to iterate over all animatable properties and see if
    // they're defined on the object. Instead we can iterate over the own
    // properties of the object and see if they're animated properties, that's
    // easier.
    let mut ids = IdVector::new(cx);
    if !unsafe { GetPropertyKeys(cx, object, JSITER_OWNONLY, ids.handle_mut()) } {
        return Err(Error::JSFailed);
    }

    // Step 6 sorts the properties by IDL attribute name, so first collect the
    // names of the animatable properties without reading any value.
    let mut names = Vec::with_capacity(ids.len());
    for id in ids.iter() {
        rooted!(&in(cx) let id = *id);

        // See if the id for the current property on the object represents a
        // animatable CSS property.
        if !id.is_string() {
            continue;
        }
        rooted!(&in(cx) let mut key_value = UndefinedValue());
        let raw_id: HandleId = id.handle().into();
        if !unsafe { JS_IdToValue(cx, *raw_id.ptr, key_value.handle_mut()) } {
            return Err(Error::JSFailed);
        }
        rooted!(&in(cx) let js_string = key_value.to_string());
        let Some(js_string) = NonNull::new(js_string.get()) else {
            return Err(Error::JSFailed);
        };
        let property_name = unsafe { jsstr_to_string(cx, js_string) };

        let Some(property_id) = lookup_css_property_by_idl_attribute_name(&property_name) else {
            continue;
        };
        debug_assert!(property_id.is_animatable());

        // The lookup table only holds names built from ASCII CSS property
        // names, so the name contains no NUL.
        let Ok(name) = CString::new(property_name) else {
            unreachable!("animatable IDL attribute names contain no NUL");
        };
        names.push((name, property_id));
    }

    // Step 6. Sort animation properties in ascending order by the Unicode
    // codepoints that define each property name. The sort key is the IDL
    // attribute name read from the object, as in the specification.
    names.sort_by(|a, b| a.0.cmp(&b.0));

    let mut properties = Vec::with_capacity(names.len());
    for (name, property_id) in names {
        // Step 7.1 Let raw value be the result of calling the [[Get]] internal
        // method on keyframe input, with property name as the property key and
        // keyframe input as the receiver.
        // Step 7.2 Check the completion record of raw value.
        rooted!(&in(cx) let mut property_value = UndefinedValue());
        if !unsafe {
            JS_GetProperty(
                cx.raw_cx(),
                object.into_handle(),
                name.as_ptr(),
                property_value.handle_mut().into_handle_mut(),
            )
        } {
            return Err(Error::JSFailed);
        }

        // Step 7.3 Convert raw value to a DOMString or to a sequence of
        // DOMStrings property values, depending on allow lists.
        let values =
            convert_single_or_list(cx, property_value.handle(), allow_lists, convert_domstring)?;

        // Step 7.4 The normalized property name is already known (see the
        // comment at the top of this function).
        // Parse each value as a value for the given animatable CSS property.
        // The specification continues to treat the value as a plain string,
        // but there's not much point.
        let declarations = values
            .iter()
            .map(|value| {
                parse_single_property_declaration(property_id, &value.str(), parser_context)
            })
            .collect();

        // Step 7.5 Add a property to keyframe output with normalized property
        // name as the property name, and property values as the property value.
        properties.push((property_id, declarations));
    }

    Ok(properties)
}

/// Parses `input` as a value for `property`, returning `None` on failure.
fn parse_single_property_declaration(
    property: NonCustomPropertyId,
    input: &str,
    parser_context: &ParserContext<'_>,
) -> Option<KeyframePropertyDeclaration> {
    let mut declaration = SourcePropertyDeclaration::default();
    let mut parser = Parser::new(input);

    // TODO: Consider reporting parse errors somewhere useful, like the devtools console.
    parser
        .parse_entirely(|parser| {
            PropertyDeclaration::parse_into(
                &mut declaration,
                PropertyId::NonCustom(property),
                parser_context,
                parser,
            )
        })
        .ok()?;

    let mut block = PropertyDeclarationBlock::new();
    block.extend(declaration.drain(), Importance::Normal);

    Some(KeyframePropertyDeclaration {
        property_id: PropertyId::NonCustom(property),
        block,
    })
}

/// This is the inverse of <https://drafts.csswg.org/web-animations-1/#animation-property-name-to-idl-attribute-name>.
fn lookup_css_property_by_idl_attribute_name(attribute_name: &str) -> Option<NonCustomPropertyId> {
    // TODO: Step 1. If property follows the <custom-property-name> production, return property.

    // Step 2. If property refers to the CSS float property, return the string "cssFloat".
    if attribute_name == "cssFloat" {
        return Some(LonghandId::Float.into());
    }

    // Step 3. If property refers to the CSS offset property, return the string "cssOffset".
    // NOTE: "offset" is not supported yet.

    // Step 4. Otherwise, return the result of applying the CSS property to IDL attribute algorithm
    // [CSSOM] to property.
    static IDL_ATTRIBUTE_TO_ANIMATED_PROPERTY_LOOKUP_TABLE: LazyLock<
        FxHashMap<String, NonCustomPropertyId>,
    > = LazyLock::new(|| {
        log::debug!("Initializing map from IDL attribute names to CSS properties");

        NonCustomPropertyId::iter()
            .filter(|non_custom_property| non_custom_property.is_animatable())
            .filter(|non_custom_property| {
                non_custom_property
                    .to_property_id()
                    .enabled_for_all_content()
            })
            .map(|non_custom_property| {
                let idl_attribute_name =
                    animation_property_name_to_idl_attribute_name(non_custom_property.name());
                (idl_attribute_name, non_custom_property)
            })
            .collect()
    });
    IDL_ATTRIBUTE_TO_ANIMATED_PROPERTY_LOOKUP_TABLE
        .get(attribute_name)
        .copied()
}

/// <https://drafts.csswg.org/web-animations-1/#animation-property-name-to-idl-attribute-name>
///
/// # Panics
/// Panics when the property name is empty or consists only of `-` characters.
/// In general it is assumed that `property_name` is a CSS property.
fn animation_property_name_to_idl_attribute_name(property_name: &str) -> String {
    let mut idl_attribute_name = String::with_capacity(property_name.len());

    let mut chunks = property_name.split('-');
    let Some(first_chunk) = chunks.next() else {
        unreachable!("CSS property name should not consist only of dashes");
    };
    idl_attribute_name.push_str(first_chunk);
    for chunk in chunks {
        let mut characters = chunk.chars();
        let Some(to_capitalize) = characters.next() else {
            continue;
        };
        idl_attribute_name.push(to_capitalize.to_ascii_uppercase());
        idl_attribute_name.push_str(characters.as_str());
    }

    idl_attribute_name
}

/// <https://drafts.csswg.org/web-animations-1/#compute-missing-keyframe-offsets>
///
/// Returns one computed offset per input keyframe.
pub(crate) fn compute_missing_keyframe_offsets(offsets: &[Option<f64>]) -> Vec<f64> {
    // Step 1. Start from the specified offsets.
    let mut computed: Vec<Option<f64>> = offsets.to_vec();
    let count = computed.len();

    // Step 2. More than one keyframe and the first has no offset: 0.
    if count > 1 && computed[0].is_none() {
        computed[0] = Some(0.0);
    }

    // Step 3. The last keyframe has no offset: 1.
    if let Some(last) = computed.last_mut() {
        if last.is_none() {
            *last = Some(1.0);
        }
    }

    // Step 4. Spread the gaps between consecutive resolved offsets evenly.
    let mut a = 0;
    while a < count {
        let Some(offset_a) = computed[a] else {
            a += 1;
            continue;
        };
        let Some(b) = (a + 1..count).find(|&index| computed[index].is_some()) else {
            break;
        };
        let offset_b = computed[b].expect("found above");
        let n = (b - a) as f64;
        for k in a + 1..b {
            computed[k] = Some(offset_a + (offset_b - offset_a) * (k - a) as f64 / n);
        }
        a = b;
    }

    computed
        .into_iter()
        .map(|offset| offset.unwrap_or(0.0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::compute_missing_keyframe_offsets;

    #[test]
    fn spreads_null_offsets_evenly_between_their_neighbours() {
        let offsets = [None, Some(0.5), None, Some(0.8), None];
        assert_eq!(
            compute_missing_keyframe_offsets(&offsets),
            vec![0.0, 0.5, 0.65, 0.8, 1.0]
        );
    }

    #[test]
    fn a_single_keyframe_sits_at_the_end() {
        assert_eq!(compute_missing_keyframe_offsets(&[None]), vec![1.0]);
        assert_eq!(compute_missing_keyframe_offsets(&[Some(0.25)]), vec![0.25]);
    }

    #[test]
    fn two_and_three_null_keyframes() {
        assert_eq!(
            compute_missing_keyframe_offsets(&[None, None]),
            vec![0.0, 1.0]
        );
        assert_eq!(
            compute_missing_keyframe_offsets(&[None, None, None]),
            vec![0.0, 0.5, 1.0]
        );
    }

    #[test]
    fn explicit_endpoints_are_kept() {
        assert_eq!(
            compute_missing_keyframe_offsets(&[Some(0.2), None, Some(0.6)]),
            vec![0.2, 0.4, 0.6]
        );
        assert_eq!(
            compute_missing_keyframe_offsets(&[Some(0.2), None]),
            vec![0.2, 1.0]
        );
    }

    #[test]
    fn no_keyframes_no_offsets() {
        assert!(compute_missing_keyframe_offsets(&[]).is_empty());
    }
}
