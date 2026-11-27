/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

use core::ops::ControlFlow;

use libjs_runtime_macros::Trace;

use crate::gc::class::{GcCell, define_cell};
use crate::gc::root::MarkedVec;
use crate::interpreter::vm::Vm;
use crate::layout::cell::Gc;
use crate::layout::object::Object;
use crate::layout::value::Value;
use crate::layout_forward::RawNativeFunctionPointer;
use crate::runtime::abstract_operations::{
    PropertyKeyGroups, group_by, ordinary_create_from_constructor, require_object_coercible,
};
use crate::runtime::array::Array;
use crate::runtime::completion::{Completion, Must, ThrowCompletionOr};
use crate::runtime::error::ErrorKind;
use crate::runtime::error_types::ErrorType;
use crate::runtime::function_object::FunctionObject;
use crate::runtime::intrinsics::Intrinsics;
use crate::runtime::iterator::get_iterator_values;
use crate::runtime::native_function::{NativeFunction, define_native_function_class, raw_native};
use crate::runtime::object::{IndexedStorageKind, IntegrityLevel, PropertyKind, ShouldThrowExceptions};
use crate::runtime::property_attributes::{Attribute, DEFAULT_ATTRIBUTES, PropertyAttributes};
use crate::runtime::property_descriptor::{from_property_descriptor, to_property_descriptor};
use crate::runtime::property_key::PropertyKey;
use crate::runtime::realm::Realm;
use crate::runtime::shape::{ASSIGN_PLAN_MAX_PROPERTIES, AssignPlan};
use crate::runtime::value::same_value;
use crate::utf16::utf16_formatted;

/// %Object.is%, which JIT code inlines with a runtime helper fallback.
pub(crate) static IS_FUNCTION: RawNativeFunctionPointer = raw_native!(ObjectConstructor::is);
/// %Object.keys%, which JIT code calls through a runtime helper.
pub(crate) static KEYS_FUNCTION: RawNativeFunctionPointer = raw_native!(ObjectConstructor::keys);
/// %Object.assign%, which JIT code calls through a runtime helper.
pub(crate) static ASSIGN_FUNCTION: RawNativeFunctionPointer = raw_native!(ObjectConstructor::assign);

#[repr(C)]
#[derive(Trace)]
pub struct ObjectConstructor {
    base: NativeFunction,
}

define_native_function_class!(
    ObjectConstructor,
    initialize: ObjectConstructor::initialize,
    call: ObjectConstructor::call,
    construct: ObjectConstructor::construct
);

impl ObjectConstructor {
    pub fn create(vm: &Vm, realm: Gc<Realm>) -> Gc<ObjectConstructor> {
        realm.create_object(vm, |_| ObjectConstructor {
            base: NativeFunction::new_with_prototype(
                vm,
                Self::CLASS,
                realm,
                realm.function_prototype(),
                Some(vm.names.Object.as_string().clone()),
            ),
        })
    }

    fn initialize(object: &Object, vm: &Vm, realm: Gc<Realm>) {
        let names = &vm.names;

        // 20.1.2.21 Object.prototype, https://tc39.es/ecma262/#sec-object.prototype
        object.define_direct_property(
            vm,
            &names.prototype,
            Value::from_object(realm.object_prototype()),
            PropertyAttributes::new(0),
        );

        let attr = PropertyAttributes::new(Attribute::WRITABLE | Attribute::CONFIGURABLE);
        let define_native_function = |property_key: &PropertyKey, function, length| {
            object.define_native_function(vm, realm, property_key, function, length, attr, None);
        };
        define_native_function(
            &names.defineProperty,
            raw_native!(ObjectConstructor::define_property),
            3,
        );
        define_native_function(
            &names.defineProperties,
            raw_native!(ObjectConstructor::define_properties),
            2,
        );
        define_native_function(&names.is, IS_FUNCTION, 2);
        define_native_function(
            &names.getOwnPropertyDescriptor,
            raw_native!(ObjectConstructor::get_own_property_descriptor),
            2,
        );
        define_native_function(
            &names.getOwnPropertyDescriptors,
            raw_native!(ObjectConstructor::get_own_property_descriptors),
            1,
        );
        define_native_function(
            &names.getOwnPropertyNames,
            raw_native!(ObjectConstructor::get_own_property_names),
            1,
        );
        define_native_function(
            &names.getOwnPropertySymbols,
            raw_native!(ObjectConstructor::get_own_property_symbols),
            1,
        );
        define_native_function(
            &names.getPrototypeOf,
            raw_native!(ObjectConstructor::get_prototype_of),
            1,
        );
        define_native_function(&names.groupBy, raw_native!(ObjectConstructor::group_by), 2);
        define_native_function(
            &names.setPrototypeOf,
            raw_native!(ObjectConstructor::set_prototype_of),
            2,
        );
        define_native_function(&names.isExtensible, raw_native!(ObjectConstructor::is_extensible), 1);
        define_native_function(&names.isFrozen, raw_native!(ObjectConstructor::is_frozen), 1);
        define_native_function(&names.isSealed, raw_native!(ObjectConstructor::is_sealed), 1);
        define_native_function(
            &names.preventExtensions,
            raw_native!(ObjectConstructor::prevent_extensions),
            1,
        );
        define_native_function(&names.freeze, raw_native!(ObjectConstructor::freeze), 1);
        define_native_function(&names.fromEntries, raw_native!(ObjectConstructor::from_entries), 1);
        define_native_function(&names.seal, raw_native!(ObjectConstructor::seal), 1);
        define_native_function(&names.keys, KEYS_FUNCTION, 1);
        define_native_function(&names.values, raw_native!(ObjectConstructor::values), 1);
        define_native_function(&names.entries, raw_native!(ObjectConstructor::entries), 1);
        define_native_function(&names.create, raw_native!(ObjectConstructor::object_create), 2);
        define_native_function(&names.hasOwn, raw_native!(ObjectConstructor::has_own), 2);
        define_native_function(&names.assign, ASSIGN_FUNCTION, 2);

        object.define_direct_property(
            vm,
            &names.length,
            Value::from_i32(1),
            PropertyAttributes::new(Attribute::CONFIGURABLE),
        );
    }

    // 20.1.1.1 Object ( [ value ] ), https://tc39.es/ecma262/#sec-object-value
    fn call(function: &NativeFunction, vm: &Vm) -> ThrowCompletionOr<Value> {
        Ok(Value::from_object(Self::construct(
            function,
            vm,
            function.as_function_object_gc(),
        )?))
    }

    // 20.1.1.1 Object ( [ value ] ), https://tc39.es/ecma262/#sec-object-value
    fn construct(function: &NativeFunction, vm: &Vm, new_target: Gc<FunctionObject>) -> ThrowCompletionOr<Gc<Object>> {
        let realm = vm.current_realm().expect("a constructor runs in a realm");
        let value = vm.argument(0);

        // 1. If NewTarget is neither undefined nor the active function object, then
        if new_target != function.as_function_object_gc() {
            // a. Return ? OrdinaryCreateFromConstructor(NewTarget, "%Object.prototype%").
            return ordinary_create_from_constructor(vm, realm, new_target, Intrinsics::object_prototype);
        }

        // 2. If value is either undefined or null, return OrdinaryObjectCreate(%Object.prototype%).
        if value.is_nullish() {
            return Ok(Object::create(vm, realm, Some(realm.object_prototype())));
        }

        // 3. Return ! ToObject(value).
        Ok(value.to_object(vm).must())
    }

    // 20.1.2.1 Object.assign ( target, ...sources ), https://tc39.es/ecma262/#sec-object.assign
    fn assign(vm: &Vm) -> ThrowCompletionOr<Value> {
        // 1. Let to be ? ToObject(target).
        let to = vm.argument(0).to_object(vm)?;

        // 2. If only one argument was passed, return to.
        if vm.argument_count() == 1 {
            return Ok(Value::from_object(to));
        }

        // 3. For each element nextSource of sources, do
        for index in 1..vm.argument_count() {
            let next_source = vm.argument(index);

            // a. If nextSource is neither undefined nor null, then
            if next_source.is_nullish() {
                continue;
            }

            // i. Let from be ! ToObject(nextSource).
            let from = next_source.to_object(vm).must();

            // OPTIMIZATION: Setting the properties of an ordinary source on an empty target that has no setters or
            //               read-only properties in the way gives it a copy of them, which can be made directly.
            if to != from && to.assign_properties_to_new_object(vm, &from) {
                continue;
            }

            // OPTIMIZATION: Snapshot named property keys and offsets without converting keys to JS strings.
            // Read current values directly while the source shape is unchanged. Getters and target setters
            // can mutate the source, so retain every original key and recheck the shape before each read.
            if try_assign_from_shape(vm, &to, &from)? {
                continue;
            }

            // ii. Let keys be ? from.[[OwnPropertyKeys]]().
            let keys = from.internal_own_property_keys(vm)?;

            // iii. For each element nextKey of keys, do
            for key_index in 0..keys.len() {
                let next_key = keys.get(key_index).expect("the index is in bounds");
                let property_key = PropertyKey::from_value(vm, next_key).must();

                // 1. Let desc be ? from.[[GetOwnProperty]](nextKey).
                let descriptor = from.internal_get_own_property(vm, &property_key)?;

                // 2. If desc is not undefined and desc.[[Enumerable]] is true, then
                let Some(descriptor) = descriptor else {
                    continue;
                };
                if !descriptor.enumerable.expect("an own property descriptor is complete") {
                    continue;
                }

                // a. Let propValue be ? Get(from, nextKey).
                let property_value = from.get(vm, &property_key)?;

                // b. Perform ? Set(to, nextKey, propValue, true).
                to.set(vm, &property_key, property_value, ShouldThrowExceptions::Yes)?;
            }
        }

        // 4. Return to.
        Ok(Value::from_object(to))
    }

    // 20.1.2.2 Object.create ( O, Properties ), https://tc39.es/ecma262/#sec-object.create
    fn object_create(vm: &Vm) -> ThrowCompletionOr<Value> {
        let realm = vm.current_realm().expect("a native function runs in a realm");

        let prototype = vm.argument(0);
        let properties = vm.argument(1);

        // 1. If Type(O) is neither Object nor Null, throw a TypeError exception.
        if !prototype.is_object() && !prototype.is_null() {
            return vm.throw_completion(ErrorKind::TypeError, ErrorType::ObjectPrototypeWrongType, &[]);
        }

        // 2. Let obj be OrdinaryObjectCreate(O).
        let object = Object::create(vm, realm, (!prototype.is_null()).then(|| prototype.as_object()));

        // 3. If Properties is not undefined, then
        if !properties.is_undefined() {
            // a. Return ? ObjectDefineProperties(obj, Properties).
            return Ok(Value::from_object(object.define_properties(vm, properties)?));
        }

        // 4. Return obj.
        Ok(Value::from_object(object))
    }

    // 20.1.2.3 Object.defineProperties ( O, Properties ), https://tc39.es/ecma262/#sec-object.defineproperties
    fn define_properties(vm: &Vm) -> ThrowCompletionOr<Value> {
        let object = vm.argument(0);
        let properties = vm.argument(1);

        // 1. If Type(O) is not Object, throw a TypeError exception.
        if !object.is_object() {
            return vm.throw_completion(ErrorKind::TypeError, ErrorType::NotAnObject, &[&"Object argument"]);
        }

        // 2. Return ? ObjectDefineProperties(O, Properties).
        Ok(Value::from_object(
            object.as_object().define_properties(vm, properties)?,
        ))
    }

    // 20.1.2.4 Object.defineProperty ( O, P, Attributes ), https://tc39.es/ecma262/#sec-object.defineproperty
    fn define_property(vm: &Vm) -> ThrowCompletionOr<Value> {
        // 1. If O is not an Object, throw a TypeError exception.
        if !vm.argument(0).is_object() {
            return vm.throw_completion(ErrorKind::TypeError, ErrorType::NotAnObject, &[&vm.argument(0)]);
        }

        let object = vm.argument(0).to_object(vm).must();

        // 2. Let key be ? ToPropertyKey(P).
        let key = vm.argument(1).to_property_key(vm)?;

        // 3. Let desc be ? ToPropertyDescriptor(Attributes).
        let mut descriptor = to_property_descriptor(vm, vm.argument(2))?;

        // 4. Perform ? DefinePropertyOrThrow(O, key, desc).
        object.define_property_or_throw(vm, &key, &mut descriptor)?;

        // 5. Return O.
        Ok(Value::from_object(object))
    }

    // 20.1.2.5 Object.entries ( O ), https://tc39.es/ecma262/#sec-object.entries
    fn entries(vm: &Vm) -> ThrowCompletionOr<Value> {
        let realm = vm.current_realm().expect("a native function runs in a realm");

        // 1. Let obj be ? ToObject(O).
        let object = vm.argument(0).to_object(vm)?;

        // OPTIMIZATION: An ordinary non-dictionary object whose own enumerable string properties are contiguous
        //               non-accessor data properties needs no per-property descriptor or [[Get]] calls.
        if object.own_property_keys_are_those_of_its_shape()
            && !object.has_intrinsic_accessors()
            && !object.shape().is_dictionary()
        {
            let shape = object.shape();
            let count = shape.property_count() as usize;
            if count == 0 {
                return Ok(Value::from_object(Array::create_empty(vm, realm)));
            }
            let (all_enumerable_contiguous, has_accessors) =
                shape.with_own_enumerable_string_keys(vm, |_, contiguous| {
                    let has_accessors = contiguous
                        && object
                            .named_properties_prefix(count)
                            .iter()
                            .any(|value| value.is_accessor());
                    (contiguous, has_accessors)
                });
            if all_enumerable_contiguous && !has_accessors {
                let entry_list = MarkedVec::with_capacity(vm, count);
                for index in 0..count {
                    let pair = Array::create_empty(vm, realm);
                    // Read key and value fresh after `Array::create_empty` in case GC moved cells.
                    let key = object.shape().cached_own_enumerable_string_key_at(index);
                    let value = object.get_direct(index as u32);
                    pair.set_indexed_property_elements(&[key, value]);
                    entry_list.push(Value::from_object(pair));
                }
                return Ok(Value::from_object(Array::create_from_list(vm, realm, &entry_list)));
            }
        }

        // 2. Let entryList be ? EnumerableOwnProperties(obj, key+value).
        let name_list = object.enumerable_own_property_names(vm, PropertyKind::KeyAndValue)?;

        // 3. Return CreateArrayFromList(entryList).
        Ok(Value::from_object(Array::create_from_list(vm, realm, &name_list)))
    }

    // 20.1.2.6 Object.freeze ( O ), https://tc39.es/ecma262/#sec-object.freeze
    fn freeze(vm: &Vm) -> ThrowCompletionOr<Value> {
        let argument = vm.argument(0);

        // 1. If O is not an Object, return O.
        if !argument.is_object() {
            return Ok(argument);
        }

        // 2. Let status be ? SetIntegrityLevel(O, frozen).
        let status = argument.as_object().set_integrity_level(vm, IntegrityLevel::Frozen)?;

        // 3. If status is false, throw a TypeError exception.
        if !status {
            return vm.throw_completion(ErrorKind::TypeError, ErrorType::ObjectFreezeFailed, &[]);
        }

        // 4. Return O.
        Ok(argument)
    }

    // 20.1.2.7 Object.fromEntries ( iterable ), https://tc39.es/ecma262/#sec-object.fromentries
    fn from_entries(vm: &Vm) -> ThrowCompletionOr<Value> {
        let realm = vm.current_realm().expect("a native function runs in a realm");

        // 1. Perform ? RequireObjectCoercible(iterable).
        let iterable = require_object_coercible(vm, vm.argument(0))?;

        // 2. Let obj be OrdinaryObjectCreate(%Object.prototype%).
        let object = Object::create(vm, realm, Some(realm.object_prototype()));

        // 3. Assert: obj is an extensible ordinary object with no own properties.

        // 4. Let closure be a new Abstract Closure with parameters (key, value) that captures obj and performs the following steps when called:
        // 5. Let adder be CreateBuiltinFunction(closure, 2, "", « »).
        // 6. Return ? AddEntriesFromIterable(obj, iterable, adder).
        let add_entry = |iterator_value: Value| -> ThrowCompletionOr<()> {
            if !iterator_value.is_object() {
                return vm.throw_completion(
                    ErrorKind::TypeError,
                    ErrorType::NotAnObject,
                    &[&utf16_formatted("Iterator value {}", &[&iterator_value])],
                );
            }

            let key = iterator_value.as_object().get(vm, &PropertyKey::from(0))?;
            let value = iterator_value.as_object().get(vm, &PropertyKey::from(1))?;

            // a. Let propertyKey be ? ToPropertyKey(key).
            let property_key = key.to_property_key(vm)?;

            // b. Perform ! CreateDataPropertyOrThrow(obj, propertyKey, value).
            object.create_data_property_or_throw(vm, &property_key, value).must();

            // c. Return undefined.
            Ok(())
        };
        get_iterator_values(vm, iterable, |iterator_value| {
            add_entry(iterator_value).err().map(Completion::from)
        })
        .into_throw_completion_or()?;

        Ok(Value::from_object(object))
    }

    // 20.1.2.8 Object.getOwnPropertyDescriptor ( O, P ), https://tc39.es/ecma262/#sec-object.getownpropertydescriptor
    fn get_own_property_descriptor(vm: &Vm) -> ThrowCompletionOr<Value> {
        // 1. Let obj be ? ToObject(O).
        let object = vm.argument(0).to_object(vm)?;

        // 2. Let key be ? ToPropertyKey(P).
        let key = vm.argument(1).to_property_key(vm)?;

        // 3. Let desc be ? obj.[[GetOwnProperty]](key).
        let descriptor = object.internal_get_own_property(vm, &key)?;

        // 4. Return FromPropertyDescriptor(desc).
        Ok(from_property_descriptor(vm, &descriptor))
    }

    // 20.1.2.9 Object.getOwnPropertyDescriptors ( O ), https://tc39.es/ecma262/#sec-object.getownpropertydescriptors
    fn get_own_property_descriptors(vm: &Vm) -> ThrowCompletionOr<Value> {
        let realm = vm.current_realm().expect("a native function runs in a realm");

        // 1. Let obj be ? ToObject(O).
        let object = vm.argument(0).to_object(vm)?;

        // 2. Let ownKeys be ? obj.[[OwnPropertyKeys]]().
        let own_keys = object.internal_own_property_keys(vm)?;

        // 3. Let descriptors be OrdinaryObjectCreate(%Object.prototype%).
        let descriptors = Object::create(vm, realm, Some(realm.object_prototype()));

        // 4. For each element key of ownKeys, do
        for index in 0..own_keys.len() {
            let key = own_keys.get(index).expect("the index is in bounds");
            let property_key = PropertyKey::from_value(vm, key).must();

            // a. Let desc be ? obj.[[GetOwnProperty]](key).
            let desc = object.internal_get_own_property(vm, &property_key)?;

            // b. Let descriptor be FromPropertyDescriptor(desc).
            let descriptor = from_property_descriptor(vm, &desc);

            // c. If descriptor is not undefined, perform ! CreateDataPropertyOrThrow(descriptors, key, descriptor).
            if !descriptor.is_undefined() {
                descriptors
                    .create_data_property_or_throw(vm, &property_key, descriptor)
                    .must();
            }
        }

        // 5. Return descriptors.
        Ok(Value::from_object(descriptors))
    }

    // 20.1.2.10 Object.getOwnPropertyNames ( O ), https://tc39.es/ecma262/#sec-object.getownpropertynames
    fn get_own_property_names(vm: &Vm) -> ThrowCompletionOr<Value> {
        let realm = vm.current_realm().expect("a native function runs in a realm");

        // 1. Return CreateArrayFromList(? GetOwnPropertyKeys(O, string)).
        // OPTIMIZATION: The string keys of an object whose own keys are those of its shape are the string keys of the
        //               shape, which the shape keeps as strings.
        if vm.argument(0).is_object() && vm.argument(0).as_object().own_property_keys_are_those_of_its_shape() {
            let object = vm.argument(0).as_object();
            let array = Array::create(vm, realm, 0, None).must();
            object
                .shape()
                .with_own_string_keys(vm, |keys, _| array.set_indexed_property_elements(keys));
            return Ok(Value::from_object(array));
        }
        let keys = get_own_property_keys(vm, vm.argument(0), GetOwnPropertyKeysType::String)?;
        Ok(Value::from_object(Array::create_from_list(vm, realm, &keys)))
    }

    // 20.1.2.11 Object.getOwnPropertySymbols ( O ), https://tc39.es/ecma262/#sec-object.getownpropertysymbols
    fn get_own_property_symbols(vm: &Vm) -> ThrowCompletionOr<Value> {
        let realm = vm.current_realm().expect("a native function runs in a realm");

        // 1. Return CreateArrayFromList(? GetOwnPropertyKeys(O, symbol)).
        let keys = get_own_property_keys(vm, vm.argument(0), GetOwnPropertyKeysType::Symbol)?;
        Ok(Value::from_object(Array::create_from_list(vm, realm, &keys)))
    }

    // 20.1.2.12 Object.getPrototypeOf ( O ), https://tc39.es/ecma262/#sec-object.getprototypeof
    fn get_prototype_of(vm: &Vm) -> ThrowCompletionOr<Value> {
        // 1. Let obj be ? ToObject(O).
        let object = vm.argument(0).to_object(vm)?;

        // 2. Return ? obj.[[GetPrototypeOf]]().
        Ok(object
            .internal_get_prototype_of(vm)?
            .map_or(Value::NULL, Value::from_object))
    }

    // 20.1.2.13 Object.groupBy ( items, callbackfn ), https://tc39.es/ecma262/#sec-object.groupby
    fn group_by(vm: &Vm) -> ThrowCompletionOr<Value> {
        let realm = vm.current_realm().expect("a native function runs in a realm");

        let items = vm.argument(0);
        let callback_function = vm.argument(1);

        // 1. Let groups be ? GroupBy(items, callbackfn, property).
        let groups: PropertyKeyGroups = group_by(vm, items, callback_function)?;

        // 2. Let obj be OrdinaryObjectCreate(null).
        let object = Object::create(vm, realm, None);

        // 3. For each Record { [[Key]], [[Elements]] } g of groups, do
        for index in 0..groups.len() {
            // a. Let elements be CreateArrayFromList(g.[[Elements]]).
            let elements = Array::create_from_list(vm, realm, groups.elements(index));

            // b. Perform ! CreateDataPropertyOrThrow(obj, g.[[Key]], elements).
            object
                .create_data_property_or_throw(vm, &groups.key(index), Value::from_object(elements))
                .must();
        }

        // 4. Return obj.
        Ok(Value::from_object(object))
    }

    // 20.1.2.14 Object.hasOwn ( O, P ), https://tc39.es/ecma262/#sec-object.hasown
    fn has_own(vm: &Vm) -> ThrowCompletionOr<Value> {
        // 1. Let obj be ? ToObject(O).
        let object = vm.argument(0).to_object(vm)?;

        // 2. Let key be ? ToPropertyKey(P).
        let key = vm.argument(1).to_property_key(vm)?;

        // 3. Return ? HasOwnProperty(obj, key).
        Ok(Value::from_bool(object.has_own_property(vm, &key)?))
    }

    // 20.1.2.15 Object.is ( value1, value2 ), https://tc39.es/ecma262/#sec-object.is
    #[allow(clippy::unnecessary_wraps, reason = "native functions can throw")]
    fn is(vm: &Vm) -> ThrowCompletionOr<Value> {
        // 1. Return SameValue(value1, value2).
        Ok(Value::from_bool(same_value(vm.argument(0), vm.argument(1))))
    }

    // 20.1.2.16 Object.isExtensible ( O ), https://tc39.es/ecma262/#sec-object.isextensible
    fn is_extensible(vm: &Vm) -> ThrowCompletionOr<Value> {
        let argument = vm.argument(0);

        // 1. If O is not an Object, return false.
        if !argument.is_object() {
            return Ok(Value::FALSE);
        }

        // 2. Return ? IsExtensible(O).
        Ok(Value::from_bool(argument.as_object().is_extensible(vm)?))
    }

    // 20.1.2.17 Object.isFrozen ( O ), https://tc39.es/ecma262/#sec-object.isfrozen
    fn is_frozen(vm: &Vm) -> ThrowCompletionOr<Value> {
        let argument = vm.argument(0);

        // 1. If O is not an Object, return true.
        if !argument.is_object() {
            return Ok(Value::TRUE);
        }

        // 2. Return ? TestIntegrityLevel(O, frozen).
        Ok(Value::from_bool(
            argument.as_object().test_integrity_level(vm, IntegrityLevel::Frozen)?,
        ))
    }

    // 20.1.2.18 Object.isSealed ( O ), https://tc39.es/ecma262/#sec-object.issealed
    fn is_sealed(vm: &Vm) -> ThrowCompletionOr<Value> {
        let argument = vm.argument(0);

        // 1. If O is not an Object, return true.
        if !argument.is_object() {
            return Ok(Value::TRUE);
        }

        // 2. Return ? TestIntegrityLevel(O, sealed).
        Ok(Value::from_bool(
            argument.as_object().test_integrity_level(vm, IntegrityLevel::Sealed)?,
        ))
    }

    // 20.1.2.19 Object.keys ( O ), https://tc39.es/ecma262/#sec-object.keys
    fn keys(vm: &Vm) -> ThrowCompletionOr<Value> {
        let realm = vm.current_realm().expect("a native function runs in a realm");

        // 1. Let obj be ? ToObject(O).
        let object = vm.argument(0).to_object(vm)?;

        // 2. Let keyList be ? EnumerableOwnProperties(obj, key).
        // OPTIMIZATION: The enumerable own keys of an object whose own keys are those of its shape are the enumerable
        //               string keys of the shape, which the shape caches as a contiguous slice of string values.
        if object.own_property_keys_are_those_of_its_shape() {
            let array = Array::create_empty(vm, realm);
            object
                .shape()
                .with_own_enumerable_string_keys(vm, |keys, _| array.set_indexed_property_elements(keys));
            return Ok(Value::from_object(array));
        }
        let name_list = object.enumerable_own_property_names(vm, PropertyKind::Key)?;

        // 3. Return CreateArrayFromList(keyList).
        Ok(Value::from_object(Array::create_from_list(vm, realm, &name_list)))
    }

    // 20.1.2.20 Object.preventExtensions ( O ), https://tc39.es/ecma262/#sec-object.preventextensions
    fn prevent_extensions(vm: &Vm) -> ThrowCompletionOr<Value> {
        let argument = vm.argument(0);

        // 1. If O is not an Object, return O.
        if !argument.is_object() {
            return Ok(argument);
        }

        // 2. Let status be ? O.[[PreventExtensions]]().
        let status = argument.as_object().internal_prevent_extensions(vm)?;

        // 3. If status is false, throw a TypeError exception.
        if !status {
            // FIXME: Improve/contextualize error message
            return vm.throw_completion(
                ErrorKind::TypeError,
                ErrorType::ObjectPreventExtensionsReturnedFalse,
                &[],
            );
        }

        // 4. Return O.
        Ok(argument)
    }

    // 20.1.2.22 Object.seal ( O ), https://tc39.es/ecma262/#sec-object.seal
    fn seal(vm: &Vm) -> ThrowCompletionOr<Value> {
        let argument = vm.argument(0);

        // 1. If O is not an Object, return O.
        if !argument.is_object() {
            return Ok(argument);
        }

        // 2. Let status be ? SetIntegrityLevel(O, sealed).
        let status = argument.as_object().set_integrity_level(vm, IntegrityLevel::Sealed)?;

        // 3. If status is false, throw a TypeError exception.
        if !status {
            return vm.throw_completion(ErrorKind::TypeError, ErrorType::ObjectSealFailed, &[]);
        }

        // 4. Return O.
        Ok(argument)
    }

    // 20.1.2.23 Object.setPrototypeOf ( O, proto ), https://tc39.es/ecma262/#sec-object.setprototypeof
    fn set_prototype_of(vm: &Vm) -> ThrowCompletionOr<Value> {
        let prototype = vm.argument(1);

        // 1. Set O to ? RequireObjectCoercible(O).
        let object = require_object_coercible(vm, vm.argument(0))?;

        // 2. If Type(proto) is neither Object nor Null, throw a TypeError exception.
        if !prototype.is_object() && !prototype.is_null() {
            return vm.throw_completion(ErrorKind::TypeError, ErrorType::ObjectPrototypeWrongType, &[]);
        }

        // 3. If Type(O) is not Object, return O.
        if !object.is_object() {
            return Ok(object);
        }

        // 4. Let status be ? O.[[SetPrototypeOf]](proto).
        let status = object
            .as_object()
            .internal_set_prototype_of(vm, (!prototype.is_null()).then(|| prototype.as_object()))?;

        // 5. If status is false, throw a TypeError exception.
        if !status {
            // FIXME: Improve/contextualize error message
            return vm.throw_completion(ErrorKind::TypeError, ErrorType::ObjectSetPrototypeOfReturnedFalse, &[]);
        }

        // 6. Return O.
        Ok(object)
    }

    // 20.1.2.24 Object.values ( O ), https://tc39.es/ecma262/#sec-object.values
    fn values(vm: &Vm) -> ThrowCompletionOr<Value> {
        let realm = vm.current_realm().expect("a native function runs in a realm");

        // 1. Let obj be ? ToObject(O).
        let object = vm.argument(0).to_object(vm)?;

        // OPTIMIZATION: An ordinary non-dictionary object with no own accessors can copy its enumerable data property
        //               values straight from named storage without descriptor checks or intermediate MarkedVec copies.
        if object.own_property_keys_are_those_of_its_shape()
            && !object.has_intrinsic_accessors()
            && !object.shape().is_dictionary()
        {
            let shape = object.shape();
            let property_count = shape.property_count();
            if property_count == 0 {
                return Ok(Value::from_object(Array::create_empty(vm, realm)));
            }
            let mut has_accessors = false;
            let mut contiguous = true;
            let mut enumerable_count = 0u32;
            shape.for_each_property_in_insertion_order(|property_key, metadata| {
                if !property_key.is_string() || !metadata.attributes.is_enumerable() {
                    contiguous = false;
                    return ControlFlow::Continue(());
                }
                if object.get_direct(metadata.offset).is_accessor() {
                    has_accessors = true;
                    return ControlFlow::Break(());
                }
                if metadata.offset != enumerable_count {
                    contiguous = false;
                }
                enumerable_count += 1;
                ControlFlow::Continue(())
            });
            if !has_accessors {
                let array = Array::create_empty(vm, realm);
                if enumerable_count > 0 {
                    if contiguous {
                        array.set_indexed_property_elements(object.named_properties_prefix(enumerable_count as usize));
                    } else {
                        array.set_indexed_property_elements_with(enumerable_count, |dst| {
                            let mut index = 0usize;
                            object
                                .shape()
                                .for_each_property_in_insertion_order(|property_key, metadata| {
                                    if property_key.is_string() && metadata.attributes.is_enumerable() {
                                        dst[index].write(object.get_direct(metadata.offset));
                                        index += 1;
                                    }
                                    ControlFlow::Continue(())
                                });
                            debug_assert_eq!(index, dst.len());
                        });
                    }
                }
                return Ok(Value::from_object(array));
            }
        }

        // 2. Let valueList be ? EnumerableOwnProperties(obj, value).
        let name_list = object.enumerable_own_property_names(vm, PropertyKind::Value)?;

        // 3. Return CreateArrayFromList(valueList).
        Ok(Value::from_object(Array::create_from_list(vm, realm, &name_list)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GetOwnPropertyKeysType {
    String,
    Symbol,
}

// 20.1.2.11.1 GetOwnPropertyKeys ( O, type ), https://tc39.es/ecma262/#sec-getownpropertykeys
fn get_own_property_keys(
    vm: &Vm,
    value: Value,
    key_type: GetOwnPropertyKeysType,
) -> ThrowCompletionOr<MarkedVec<'_, Value>> {
    // 1. Let obj be ? ToObject(O).
    let object = value.to_object(vm)?;

    // 2. Let keys be ? obj.[[OwnPropertyKeys]]().
    let keys = object.internal_own_property_keys(vm)?;

    // 3. Let nameList be a new empty List.
    let name_list = MarkedVec::new(vm);

    // 4. For each element nextKey of keys, do
    for index in 0..keys.len() {
        let next_key = keys.get(index).expect("the index is in bounds");

        // a. If Type(nextKey) is Symbol and type is symbol or Type(nextKey) is String and type is string, then
        if (next_key.is_symbol() && key_type == GetOwnPropertyKeysType::Symbol)
            || (next_key.is_string() && key_type == GetOwnPropertyKeysType::String)
        {
            // i. Append nextKey as the last element of nameList.
            name_list.push(next_key);
        }
    }

    // 5. Return nameList.
    Ok(name_list)
}

fn source_eligible_for_shape_assign(source: &Object) -> bool {
    source.eligible_for_own_property_enumeration_fast_path()
        && !source.has_intrinsic_accessors()
        && !source.may_interfere_with_indexed_property_access()
        && !source.has_parameter_map()
        && !source.is_ecmascript_function_object()
        && source.indexed_storage_kind() == IndexedStorageKind::None
        && !source.shape().is_dictionary()
}

/// Applies `plan` from `source` onto `target` if no copied source slot or overwritten target slot holds an accessor.
/// Returns `true` if applied, or `false` having performed no mutations.
#[inline]
fn try_apply_assign_plan(
    vm: &Vm,
    target: &Object,
    source: &Object,
    target_shape: crate::layout::cell::Gc<crate::runtime::shape::Shape>,
    plan: &AssignPlan,
) -> bool {
    let Some(result_shape) = plan.result_shape else {
        return false;
    };
    let adds_properties = result_shape != target_shape;
    if adds_properties {
        if target_shape.is_non_extensible()
            || target.requires_slow_add_own_property()
            || result_shape.has_watched_slots()
            || result_shape.is_prototype_shape()
        {
            return false;
        }
        if target_shape.prototype().is_some()
            && !plan.prototype_validity.is_some_and(|validity| validity.is_valid())
        {
            return false;
        }
    }
    let count = plan.copy_count as usize;
    let existing_count = plan.target_property_count;
    for &(src_off, dst_off) in &plan.copies[..count] {
        if source.get_direct(u32::from(src_off)).is_accessor() {
            return false;
        }
        if dst_off < existing_count && target.get_direct(u32::from(dst_off)).is_accessor() {
            return false;
        }
    }
    if adds_properties {
        target.prepare_for_shape_transition(result_shape.property_count());
        target_shape.mark_unstable(vm);
    }
    for &(src_off, dst_off) in &plan.copies[..count] {
        target.write_direct_from_assign(u32::from(dst_off), source.get_direct(u32::from(src_off)));
    }
    if adds_properties {
        target.finish_shape_transition(result_shape);
    }
    true
}

/// Fast `Object.assign(target, src1, src2, src3)` helper for JIT code. Runs no JS code and allocates no GC cells;
/// returns `None` having done nothing observable if any source needs the slow path or misses shape caches.
pub(crate) fn object_assign_running_no_code(
    vm: &Vm,
    target_val: Value,
    src1: Value,
    src2: Value,
    src3: Value,
) -> Option<Value> {
    if !target_val.is_object() {
        return None;
    }
    let target = target_val.as_object();
    let realm = vm.current_realm()?;
    let initial_shape = target.shape();
    if !target.has_ordinary_named_property_lookup_and_set()
        || target.has_intrinsic_accessors()
        || initial_shape.is_dictionary()
        || initial_shape.has_watched_slots()
        || initial_shape.is_prototype_shape()
        || target.has_global_object_flag()
        || initial_shape.realm() != realm
    {
        return None;
    }

    let raw_sources = [src1, src2, src3];
    let mut sources: [Option<Gc<Object>>; 3] = [None; 3];
    let mut source_count = 0usize;
    for src in raw_sources {
        if src.is_nullish() {
            continue;
        }
        if !src.is_object() {
            return None;
        }
        let source = src.as_object();
        if source == target || !source_eligible_for_shape_assign(&source) {
            return None;
        }
        if source.shape().property_count() == 0 {
            continue;
        }
        sources[source_count] = Some(source);
        source_count += 1;
    }
    if source_count == 0 {
        return Some(target_val);
    }

    #[derive(Clone, Copy)]
    enum Step {
        Identical { source: Gc<Object>, count: usize },
        Plan { source: Gc<Object>, plan_index: usize },
    }

    let new_object_shape = realm.new_object_shape();
    let obj_proto = new_object_shape.prototype();
    let obj_proto_validity = obj_proto.and_then(|proto| proto.shape().prototype_chain_validity());
    let target_can_use_identical = target.indexed_storage_kind() == IndexedStorageKind::None
        && target.eligible_for_own_property_enumeration_fast_path()
        && !target.may_interfere_with_indexed_property_access()
        && !target.requires_slow_add_own_property();
    let initial_property_count = initial_shape.property_count();
    let initial_target_ptr = target.named_properties_ptr();
    let mut plan_entries: Option<core::cell::Ref<'_, [AssignPlan]>> = None;

    let mut steps: [Option<Step>; 3] = [None; 3];
    let mut cur_shape = initial_shape;

    for (step_idx, source_slot) in sources[..source_count].iter().enumerate() {
        let source = source_slot.expect("source slot is populated");
        let source_shape = source.shape();

        // First try the identical-layout fast copy when the target is still an empty new object.
        if cur_shape == new_object_shape
            && target_can_use_identical
            && source_shape.realm() == realm
            && let Some(proto) = obj_proto
            && let Some(proto_validity) = obj_proto_validity
            && let Some(assign_copy) = source_shape.cached_assign_copy()
            && assign_copy.identical_layout
            && assign_copy.prototype_validity == proto_validity
            && proto_validity.is_valid()
            && assign_copy.copy_shape.prototype() == Some(proto)
            && !assign_copy.copy_shape.has_watched_slots()
            && !assign_copy.copy_shape.is_prototype_shape()
        {
            let count = source_shape.property_count() as usize;
            if source
                .named_properties_prefix(count)
                .iter()
                .any(|value| value.is_accessor())
            {
                return None;
            }
            steps[step_idx] = Some(Step::Identical { source, count });
            cur_shape = assign_copy.copy_shape;
            continue;
        }

        // Otherwise consult the VM's cached (cur_shape, source_shape) AssignPlan by index without copying it.
        let entries = plan_entries.get_or_insert_with(|| vm.assign_plan_cache().borrow_entries());
        let plan_index = crate::runtime::shape::AssignPlanCache::find_index(entries, cur_shape, source_shape)?;
        let plan = &entries[plan_index];
        let result_shape = plan.result_shape?;
        if result_shape != cur_shape {
            if initial_shape.is_non_extensible()
                || target.requires_slow_add_own_property()
                || result_shape.has_watched_slots()
                || result_shape.is_prototype_shape()
            {
                return None;
            }
            if cur_shape.prototype().is_some()
                && !plan.prototype_validity.is_some_and(|validity| validity.is_valid())
            {
                return None;
            }
        }
        let count = plan.copy_count as usize;
        let src_ptr = source.named_properties_ptr();
        for &(src_off, dst_off) in &plan.copies[..count] {
            // SAFETY: `src_off` is a valid own property offset of `source_shape`.
            if unsafe { *src_ptr.add(usize::from(src_off)) }.is_accessor() {
                return None;
            }
            // Only slots that existed on `target` before this call could hold accessors;
            // slots added by earlier steps in this call came from verified non-accessor values.
            if u32::from(dst_off) < initial_property_count {
                // SAFETY: `dst_off < initial_property_count <= target.named_storage_capacity()`.
                if unsafe { *initial_target_ptr.add(usize::from(dst_off)) }.is_accessor() {
                    return None;
                }
            }
        }
        steps[step_idx] = Some(Step::Plan { source, plan_index });
        cur_shape = result_shape;
    }

    // All steps validated with zero mutations and zero GC allocations. Apply in a single batch.
    if cur_shape != initial_shape {
        target.prepare_for_shape_transition(cur_shape.property_count());
        initial_shape.mark_unstable(vm);
    }
    let dst_ptr = target.named_properties_ptr();
    for step in steps[..source_count].iter().flatten() {
        match *step {
            Step::Identical { source, count } => {
                // SAFETY: `prepare_for_shape_transition` ensured capacity >= `count`, `source != target`,
                //         and `count <= source.named_storage_capacity()`.
                unsafe {
                    core::ptr::copy_nonoverlapping(source.named_properties_ptr(), dst_ptr, count);
                }
            }
            Step::Plan { source, plan_index } => {
                let plan = &plan_entries.as_deref().expect("plan entries borrowed for Plan step")[plan_index];
                let src_ptr = source.named_properties_ptr();
                for &(src_off, dst_off) in &plan.copies[..plan.copy_count as usize] {
                    // SAFETY: `prepare_for_shape_transition` ensured target capacity >= `cur_shape.property_count()`,
                    //         `src_off` and `dst_off` are validated own offsets, and `target` is unwatched non-global.
                    unsafe {
                        *dst_ptr.add(usize::from(dst_off)) = *src_ptr.add(usize::from(src_off));
                    }
                }
            }
        }
    }
    if cur_shape != initial_shape {
        target.finish_shape_transition(cur_shape);
    }
    Some(target_val)
}

fn try_assign_from_shape(vm: &Vm, target: &Object, source: &Object) -> ThrowCompletionOr<bool> {
    let source_shape = source.shape();
    if !source_eligible_for_shape_assign(source) {
        return Ok(false);
    }

    let source_property_count = source_shape.property_count();
    if source_property_count == 0 {
        return Ok(true);
    }

    let target_shape = target.shape();
    let target_ordinary_unwatched = target.has_ordinary_named_property_lookup_and_set()
        && !target.has_intrinsic_accessors()
        && !target_shape.has_watched_slots()
        && !target_shape.is_prototype_shape()
        && !target.has_global_object_flag();

    // OPTIMIZATION: Consult or build a cached `(target_shape, source_shape) -> AssignPlan` that handles both
    //               overwriting existing writable own properties and appending new default data properties in one
    //               shape transition, without allocating a MarkedVec or repeating Shape::lookup calls.
    if target_ordinary_unwatched && !target_shape.is_dictionary() {
        if !core::ptr::eq(target, source)
            && target_shape.realm() == source_shape.realm()
            && let Some(plan) = vm.assign_plan_cache().get(target_shape, source_shape)
            && try_apply_assign_plan(vm, target, source, target_shape, &plan)
        {
            return Ok(true);
        }

        if target_shape == source_shape {
            let mut fast_ok = true;
            let can_cache = !core::ptr::eq(target, source)
                && (source_property_count as usize) <= ASSIGN_PLAN_MAX_PROPERTIES
                && target_shape.property_count() <= u16::MAX as u32;
            let mut copies = [(0u16, 0u16); ASSIGN_PLAN_MAX_PROPERTIES];
            let mut copy_count = 0usize;
            source_shape.for_each_property_in_insertion_order(|property_key, metadata| {
                if !property_key.is_string() {
                    fast_ok = false;
                    return ControlFlow::Break(());
                }
                if metadata.attributes.is_enumerable() {
                    if !metadata.attributes.is_writable()
                        || source.get_direct(metadata.offset).is_accessor()
                        || target.get_direct(metadata.offset).is_accessor()
                    {
                        fast_ok = false;
                        return ControlFlow::Break(());
                    }
                    if can_cache && metadata.offset <= u16::MAX as u32 {
                        copies[copy_count] = (metadata.offset as u16, metadata.offset as u16);
                        copy_count += 1;
                    }
                }
                ControlFlow::Continue(())
            });
            if fast_ok {
                if can_cache {
                    vm.assign_plan_cache().set(AssignPlan {
                        target_shape: Some(target_shape),
                        source_shape: Some(source_shape),
                        result_shape: Some(target_shape),
                        prototype_validity: None,
                        target_property_count: target_shape.property_count() as u16,
                        copy_count: copy_count as u8,
                        copies,
                    });
                }
                if !core::ptr::eq(target, source) {
                    if can_cache {
                        for &(src_off, dst_off) in &copies[..copy_count] {
                            target.write_direct_from_assign(u32::from(dst_off), source.get_direct(u32::from(src_off)));
                        }
                    } else {
                        source_shape.for_each_property_in_insertion_order(|_, metadata| {
                            if metadata.attributes.is_enumerable() {
                                target.put_direct_from_cache(metadata.offset, source.get_direct(metadata.offset));
                            }
                            ControlFlow::Continue(())
                        });
                    }
                }
                return Ok(true);
            }
        } else if !core::ptr::eq(target, source)
            && target_shape.realm() == source_shape.realm()
            && (source_property_count as usize) <= ASSIGN_PLAN_MAX_PROPERTIES
            && target_shape.property_count() <= u16::MAX as u32
        {
            let target_property_count = target_shape.property_count();
            let mut copies = [(0u16, 0u16); ASSIGN_PLAN_MAX_PROPERTIES];
            let mut copy_count = 0usize;
            #[cfg_attr(
                ladybird_gc_lints,
                expect(
                    gc_container_on_stack,
                    reason = "only string property keys from source_shape are pushed"
                )
            )]
            let mut new_keys: Vec<PropertyKey> = Vec::new();
            let mut next_new_offset = target_property_count;
            let mut fast_ok = true;
            let mut can_cache_plan = true;

            source_shape.for_each_property_in_insertion_order(|property_key, metadata| {
                if !property_key.is_string() || metadata.offset > u16::MAX as u32 {
                    fast_ok = false;
                    return ControlFlow::Break(());
                }
                if !metadata.attributes.is_enumerable() {
                    return ControlFlow::Continue(());
                }
                if source.get_direct(metadata.offset).is_accessor() {
                    fast_ok = false;
                    return ControlFlow::Break(());
                }
                if let Some(target_metadata) = target_shape.lookup(property_key) {
                    if !target_metadata.attributes.is_writable()
                        || target_metadata.offset > u16::MAX as u32
                        || target.get_direct(target_metadata.offset).is_accessor()
                    {
                        fast_ok = false;
                        return ControlFlow::Break(());
                    }
                    copies[copy_count] = (metadata.offset as u16, target_metadata.offset as u16);
                    copy_count += 1;
                } else {
                    if next_new_offset > u16::MAX as u32 {
                        fast_ok = false;
                        return ControlFlow::Break(());
                    }
                    copies[copy_count] = (metadata.offset as u16, next_new_offset as u16);
                    copy_count += 1;
                    next_new_offset += 1;
                    new_keys.push(property_key.clone());
                }
                ControlFlow::Continue(())
            });

            if fast_ok {
                if new_keys.is_empty() {
                    let plan = AssignPlan {
                        target_shape: Some(target_shape),
                        source_shape: Some(source_shape),
                        result_shape: Some(target_shape),
                        prototype_validity: None,
                        target_property_count: target_property_count as u16,
                        copy_count: copy_count as u8,
                        copies,
                    };
                    vm.assign_plan_cache().set(plan);
                    for &(src_off, dst_off) in &copies[..copy_count] {
                        target.write_direct_from_assign(u32::from(dst_off), source.get_direct(u32::from(src_off)));
                    }
                    return Ok(true);
                } else if !target_shape.is_non_extensible() && !target.requires_slow_add_own_property() {
                    let proto = target_shape.prototype();
                    let current_realm = vm.current_realm();
                    let prototype_validity = match proto {
                        None => None,
                        Some(prototype) => {
                            let is_object_proto = current_realm
                                .is_some_and(|realm| prototype == realm.object_prototype());
                            let validity = prototype.shape().prototype_chain_validity();
                            if !is_object_proto || !validity.is_some_and(|v| v.is_valid()) {
                                can_cache_plan = false;
                            }
                            validity
                        }
                    };
                    if target.set_of_keys_create_them(vm, &new_keys) {
                        let mut result_shape = target_shape;
                        for property_key in &new_keys {
                            result_shape = result_shape.create_put_transition(vm, property_key, DEFAULT_ATTRIBUTES);
                        }
                        if !result_shape.has_watched_slots() && !result_shape.is_prototype_shape() {
                            if can_cache_plan {
                                let plan = AssignPlan {
                                    target_shape: Some(target_shape),
                                    source_shape: Some(source_shape),
                                    result_shape: Some(result_shape),
                                    prototype_validity,
                                    target_property_count: target_property_count as u16,
                                    copy_count: copy_count as u8,
                                    copies,
                                };
                                vm.assign_plan_cache().set(plan);
                            }
                            target.prepare_for_shape_transition(result_shape.property_count());
                            target_shape.mark_unstable(vm);
                            for &(src_off, dst_off) in &copies[..copy_count] {
                                target.write_direct_from_assign(
                                    u32::from(dst_off),
                                    source.get_direct(u32::from(src_off)),
                                );
                            }
                            target.finish_shape_transition(result_shape);
                            return Ok(true);
                        }
                    }
                }
            }
        }
    }

    // The key, storage offset and enumerability of each property, read out first since nothing may allocate while
    // the shape is borrowed.
    let properties: MarkedVec<(PropertyKey, u32, bool)> = MarkedVec::with_capacity(vm, source_property_count as usize);
    let mut has_non_string_keys = false;
    let mut source_has_accessors = false;
    source_shape.for_each_property_in_insertion_order(|property_key, metadata| {
        if !property_key.is_string() {
            has_non_string_keys = true;
            return ControlFlow::Break(());
        }
        let is_enumerable = metadata.attributes.is_enumerable();
        if is_enumerable && source.get_direct(metadata.offset).is_accessor() {
            source_has_accessors = true;
        }
        properties.push((property_key.clone(), metadata.offset, is_enumerable));
        ControlFlow::Continue(())
    });
    if has_non_string_keys {
        return Ok(false);
    }

    let mut can_define_direct_new = !source_has_accessors
        && target_ordinary_unwatched
        && !target.requires_slow_add_own_property()
        && !target_shape.is_non_extensible()
        && target.set_of_properties_of_creates_them(vm, source);

    for index in 0..properties.len() {
        let (property_key, offset, is_enumerable) = properties.get(index).expect("the index is in bounds");
        let mut value;
        if source.shape() == source_shape {
            if !is_enumerable {
                continue;
            }
            value = source.get_direct(offset);
            if value.is_accessor() {
                can_define_direct_new = false;
                value = source.get(vm, &property_key)?;
            }
        } else {
            can_define_direct_new = false;
            let descriptor = source.internal_get_own_property(vm, &property_key)?;
            if !descriptor
                .is_some_and(|descriptor| descriptor.enumerable.expect("an own property descriptor is complete"))
            {
                continue;
            }
            value = source.get(vm, &property_key)?;
        }
        // OPTIMIZATION: [[Set]] of an own writable data property of an object with ordinary property lookup and [[Set]]
        //               changes its value, and nothing else. When no accessors can run and target's prototype chain has
        //               no blocking setters/read-only properties, missing properties can also be added directly.
        if target.has_ordinary_named_property_lookup_and_set() && !target.has_intrinsic_accessors() {
            if let Some(metadata) = target.shape().lookup(&property_key) {
                if metadata.attributes.is_writable() && !target.get_direct(metadata.offset).is_accessor() {
                    target.replace_direct(vm, metadata.offset, value);
                    continue;
                }
            } else if can_define_direct_new {
                target.define_direct_property(vm, &property_key, value, DEFAULT_ATTRIBUTES);
                continue;
            }
        }
        can_define_direct_new = false;
        target.set(vm, &property_key, value, ShouldThrowExceptions::Yes)?;
    }
    Ok(true)
}
