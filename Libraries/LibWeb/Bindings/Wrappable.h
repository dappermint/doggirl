/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

#pragma once

#include <AK/Optional.h>
#include <AK/String.h>
#include <AK/TypeCasts.h>
#include <AK/Utf16FlyString.h>
#include <AK/Vector.h>
#include <LibGC/Cell.h>
#include <LibGC/Ptr.h>
#include <LibGC/Weak.h>
#include <LibJS/Forward.h>
#include <LibJS/Heap/Cell.h>
#include <LibJS/Runtime/HostObject.h>
#include <LibJS/Runtime/Object.h>
#include <LibJS/Runtime/Value.h>
#include <LibURL/Origin.h>
#include <LibWeb/Bindings/IntrinsicDefinitions.h>
#include <LibWeb/Export.h>
#include <LibWeb/Forward.h>

namespace Web::Bindings {

template<typename Implementation, typename Call>
decltype(auto) invoke_first_available(Implementation& implementation, Call&& call)
{
    return call(implementation);
}

template<typename Implementation, typename FirstCall, typename SecondCall, typename... RemainingCalls>
decltype(auto) invoke_first_available(Implementation& implementation, FirstCall&& first_call, SecondCall&& second_call, RemainingCalls&&... remaining_calls)
{
    if constexpr (requires { first_call(implementation); })
        return first_call(implementation);
    else
        return invoke_first_available(implementation, second_call, remaining_calls...);
}

template<typename Implementation, typename Call>
decltype(auto) invoke_first_available_static(Call&& call)
{
    return call.template operator()<Implementation>();
}

template<typename Implementation, typename FirstCall, typename SecondCall, typename... RemainingCalls>
decltype(auto) invoke_first_available_static(FirstCall&& first_call, SecondCall&& second_call, RemainingCalls&&... remaining_calls)
{
    if constexpr (requires { first_call.template operator()<Implementation>(); })
        return first_call.template operator()<Implementation>();
    else
        return invoke_first_available_static<Implementation>(second_call, remaining_calls...);
}

// The realm passed to these helpers is the preferred realm for wrapper allocation.
// Wrapper identity is keyed by the caller's WrapperWorld.
WEB_API GC::Ref<JS::HostObject> create_global_object_wrapper(JS::Realm& wrapper_realm, GC::Ref<Wrappable>);
WEB_API GC::Ref<JS::HostObject> create_wrapper_for_wrappable(JS::Realm& wrapper_realm, GC::Ref<Wrappable>);
WEB_API JS::Realm& wrapper_realm_for_wrappable(WrapperWorld const& wrapper_world, JS::Realm& preferred_realm, GC::Ref<Wrappable>);
WEB_API JS::Realm& wrapper_realm_for_node(WrapperWorld const& wrapper_world, JS::Realm& preferred_realm, DOM::Node&);
WEB_API GC::Ref<JS::HostObject> wrap(WrapperWorld& wrapper_world, JS::Realm& preferred_realm, GC::Ref<Wrappable>);
WEB_API JS::ThrowCompletionOr<void> set_prototype_of_cached_main_world_wrapper(Wrappable&, JS::Object&);
WEB_API void preserve_wrapper(Wrappable&, JS::HostObject&);
WEB_API bool wrapper_is_preserved(JS::HostObject const&);

#ifndef WEB_WRAPPABLE
#    define WEB_WRAPPABLE(class_, base_class)                                               \
        GC_CELL(class_, base_class)                                                         \
    public:                                                                                 \
        using Base::initialize;                                                             \
        static constexpr auto s_interface_name = Bindings::InterfaceName::class_;           \
        virtual Bindings::InterfaceName interface_name() const override                     \
        {                                                                                   \
            return s_interface_name;                                                        \
        }                                                                                   \
        virtual bool implements_interface(Bindings::InterfaceName interface) const override \
        {                                                                                   \
            if (interface == s_interface_name)                                              \
                return true;                                                                \
            return Base::implements_interface(interface);                                   \
        }                                                                                   \
        virtual bool implements_interface(                                                  \
            String const& interface) const override                                         \
        {                                                                                   \
            if (interface == #class_)                                                       \
                return true;                                                                \
            return Base::implements_interface(interface);                                   \
        }                                                                                   \
                                                                                            \
    public:
#endif

#ifndef WEB_NON_IDL_WRAPPABLE
#    define WEB_NON_IDL_WRAPPABLE(class_, base_class) \
        GC_CELL(class_, base_class)                   \
    public:                                           \
        using Base::initialize;
#endif

// Base class for internal implementation objects that can be reflected into JS
// by a wrapper, a host object that holds the implementation object in its wrappable slot.
class WEB_API Wrappable : public JS::Cell {
    GC_CELL(Wrappable, JS::Cell);

public:
    using JSValueConversionIsForbidden = void;

    virtual ~Wrappable() override = default;

    [[nodiscard]] virtual Bindings::InterfaceName interface_name() const { VERIFY_NOT_REACHED(); }
    [[nodiscard]] virtual bool implements_interface(Bindings::InterfaceName) const { return false; }
    [[nodiscard]] virtual bool implements_interface(String const&) const { return false; }

    // Whether an object of an interface implemented in Rust owns this object, so that its wrappers are the Rust
    // object's (Libraries/LibWeb/Bindings/Rust/src/bindings/event_target.rs).
    [[nodiscard]] virtual bool is_owned_by_rust_interface_object() const { return false; }

    enum WrappableFlag : u8 {
        MainWorldWrapperIsPreserved = 1 << 0,
        HasNonMainWorldPreservedWrappers = 1 << 1,
        IsEventTarget = 1 << 2,
        IsNode = 1 << 3,
        IsElement = 1 << 4,
        IsHTMLElement = 1 << 5,
        IsSVGElement = 1 << 6,
        IsDocument = 1 << 7,
    };

    [[nodiscard]] bool is_event_target_wrappable() const { return (m_flags & IsEventTarget) != 0; }
    [[nodiscard]] bool is_node_wrappable() const { return (m_flags & IsNode) != 0; }
    [[nodiscard]] bool is_element_wrappable() const { return (m_flags & IsElement) != 0; }
    [[nodiscard]] bool is_html_element_wrappable() const { return (m_flags & IsHTMLElement) != 0; }
    [[nodiscard]] bool is_svg_element_wrappable() const { return (m_flags & IsSVGElement) != 0; }
    [[nodiscard]] bool is_document_wrappable() const { return (m_flags & IsDocument) != 0; }

    template<typename T>
    requires(IsBaseOf<Wrappable, T> && requires { T::s_interface_name; })
    bool fast_is() const
    {
        if constexpr (T::s_interface_name == Bindings::InterfaceName::EventTarget)
            return is_event_target_wrappable();
        else if constexpr (T::s_interface_name == Bindings::InterfaceName::Node)
            return is_node_wrappable();
        else if constexpr (T::s_interface_name == Bindings::InterfaceName::Element)
            return is_element_wrappable();
        else if constexpr (T::s_interface_name == Bindings::InterfaceName::HTMLElement)
            return is_html_element_wrappable();
        else if constexpr (T::s_interface_name == Bindings::InterfaceName::SVGElement)
            return is_svg_element_wrappable();
        else if constexpr (T::s_interface_name == Bindings::InterfaceName::Document)
            return is_document_wrappable();
        else
            return implements_interface(T::s_interface_name);
    }

    template<typename T>
    requires(IsBaseOf<Wrappable, T> && requires { T::s_interface_name; })
    T* fast_as()
    {
        if (!fast_is<T>())
            return nullptr;
        return static_cast<T*>(this);
    }

    template<typename T>
    requires(IsBaseOf<Wrappable, T> && requires { T::s_interface_name; })
    T const* fast_as() const
    {
        if (!fast_is<T>())
            return nullptr;
        return static_cast<T const*>(this);
    }

    // https://html.spec.whatwg.org/multipage/browsers.html#extract-an-origin
    // A wrapper's extract an origin operation is its implementation object's.
    virtual Optional<URL::Origin> extract_an_origin() const;

    // Wrappers forward legacy platform object enumeration hooks here.
    virtual Vector<Utf16FlyString> supported_property_names() const;
    virtual bool is_supported_property_name(Utf16FlyString const& name) const;

    [[nodiscard]] virtual GC::Ptr<Wrappable> relevant_global_impl() const { return nullptr; }

    static constexpr size_t main_world_wrapper_offset() { return offsetof(Wrappable, m_main_world_wrapper); }

protected:
    Wrappable();

    void set_wrappable_flag(WrappableFlag flag) { m_flags |= flag; }

    [[nodiscard]] GC::Ptr<JS::HostObject> cached_main_world_wrapper() const;
    [[nodiscard]] GC::Ptr<JS::HostObject> cached_main_world_wrapper(WrapperWorld const&) const;

    virtual GC::Ref<JS::HostObject> create_wrapper(JS::Realm& wrapper_realm);
    virtual void visit_edges(GC::Cell::Visitor&) override;

private:
    friend class GCAllocatedWrappable;
    friend class WrapperWorld;
    friend WEB_API GC::Ref<JS::HostObject> create_global_object_wrapper(JS::Realm& wrapper_realm, GC::Ref<Wrappable>);
    friend WEB_API JS::Realm& wrapper_realm_for_wrappable(WrapperWorld const&, JS::Realm&, GC::Ref<Wrappable>);
    friend WEB_API GC::Ref<JS::HostObject> wrap(WrapperWorld& wrapper_world, JS::Realm& preferred_realm, GC::Ref<Wrappable>);
    friend WEB_API JS::ThrowCompletionOr<void> set_prototype_of_cached_main_world_wrapper(Wrappable&, JS::Object&);
    friend WEB_API void preserve_wrapper(Wrappable&, JS::HostObject&);
    friend WEB_API bool wrapper_is_preserved(JS::HostObject const&);

    void set_cached_main_world_wrapper(JS::HostObject&);
    void clear_cached_main_world_wrapper(JS::HostObject const&);

    // Main-world preservation intentionally survives document navigation: an
    // adopted node may still be observed through its original wrapper realm,
    // and clearing this edge would make wrapper state depend on bfcache-like
    // resurrection timing. Non-main-world entries are removed by detach().
    GC::Weak<JS::HostObject> m_main_world_wrapper;
    u8 m_flags { 0 };
};

static_assert(sizeof(Wrappable) == 32);

// Current LibWeb implementation objects are GC-owned. Keeping that ownership
// and its preservation flags in a separate base lets classes migrate to other
// ownership models individually.
class WEB_API GCAllocatedWrappable : public Wrappable {
    WEB_NON_IDL_WRAPPABLE(GCAllocatedWrappable, Wrappable);

public:
    virtual ~GCAllocatedWrappable() override;

protected:
    GCAllocatedWrappable() = default;

    virtual void visit_edges(GC::Cell::Visitor&) override;

private:
    friend class WrapperWorld;
    friend WEB_API void preserve_wrapper(Wrappable&, JS::HostObject&);
    friend WEB_API bool wrapper_is_preserved(JS::HostObject const&);

    bool main_world_wrapper_is_preserved() const { return (m_flags & MainWorldWrapperIsPreserved) != 0; }
    void set_main_world_wrapper_is_preserved(bool value)
    {
        if (value)
            m_flags |= MainWorldWrapperIsPreserved;
        else
            m_flags &= ~MainWorldWrapperIsPreserved;
    }
    bool has_non_main_world_preserved_wrappers() const { return (m_flags & HasNonMainWorldPreservedWrappers) != 0; }
    void set_has_non_main_world_preserved_wrappers(bool value)
    {
        if (value) {
            m_flags |= HasNonMainWorldPreservedWrappers;
            mark_needs_destruction();
        } else {
            m_flags &= ~HasNonMainWorldPreservedWrappers;
        }
    }
    void clear_preserved_wrappers(WrapperWorld const&);
};

static_assert(sizeof(GCAllocatedWrappable) == 32);
// GCAllocatedWrappable's dsize is 25 bytes, allowing derived classes to reuse
// the remaining tail padding where supported by the ABI.

static_assert(!IsConstructible<JS::Value, Wrappable*>);
static_assert(!IsConstructible<JS::Value, Wrappable const*>);
static_assert(!IsConstructible<JS::Value, GC::Ptr<Wrappable>>);
static_assert(!IsConstructible<JS::Value, GC::Ref<Wrappable>>);
static_assert(!IsConstructible<JS::Value, GC::Root<Wrappable> const&>);

template<typename T>
requires(IsBaseOf<Wrappable, T> && !IsSameIgnoringCV<T, Wrappable>)
[[nodiscard]] GC::Ref<JS::HostObject> create_global_object_wrapper(JS::Realm& wrapper_realm, GC::Ref<T> wrappable)
{
    return create_global_object_wrapper(wrapper_realm, GC::Ref<Wrappable> { wrappable });
}

WEB_API GC::Ptr<JS::HostObject> wrap(WrapperWorld& wrapper_world, JS::Realm& preferred_realm, GC::Ptr<Wrappable>);

template<typename T>
requires(IsBaseOf<Wrappable, T> && !IsSameIgnoringCV<T, Wrappable>)
[[nodiscard]] GC::Ref<JS::HostObject> wrap(WrapperWorld& wrapper_world, JS::Realm& preferred_realm, GC::Ref<T> wrappable)
{
    return wrap(wrapper_world, preferred_realm, GC::Ref<Wrappable> { wrappable });
}

template<typename T>
requires(IsBaseOf<Wrappable, T> && !IsSameIgnoringCV<T, Wrappable>)
[[nodiscard]] GC::Ptr<JS::HostObject> wrap(WrapperWorld& wrapper_world, JS::Realm& preferred_realm, GC::Ptr<T> wrappable)
{
    return wrap(wrapper_world, preferred_realm, GC::Ptr<Wrappable> { wrappable });
}

WEB_API void cache_global_object_wrapper(JS::Realm&);
WEB_API Wrappable* wrappable_impl_from(JS::Object*);
WEB_API Wrappable const* wrappable_impl_from(JS::Object const*);

template<typename Impl, typename Object>
requires(IsBaseOf<Wrappable, Impl>)
[[nodiscard]] CopyConst<Object, Impl>* impl_from(Object* object)
{
    if (!object)
        return nullptr;

    if (auto* wrappable = wrappable_impl_from(object))
        return as_if<Impl>(wrappable);

    return nullptr;
}

template<typename Impl, typename Object>
[[nodiscard]] CopyConst<Object, Impl>& impl_from(Object& object)
{
    auto* impl = impl_from<Impl>(&object);
    VERIFY(impl);
    return *impl;
}

}
