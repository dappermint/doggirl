/*
 * Copyright (c) 2022, Andreas Kling <andreas@ladybird.org>
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

#include <AK/HashTable.h>
#include <AK/NeverDestroyed.h>
#include <AK/Utf16StringBuilder.h>
#include <LibWeb/DOM/QualifiedName.h>

namespace Web::DOM {

static unsigned hash_impl(Utf16FlyString const& local_name, Optional<Utf16FlyString> const& prefix, Optional<Utf16FlyString> const& namespace_)
{
    unsigned hash = ptr_hash(local_name.raw_identity());
    if (prefix.has_value())
        hash = pair_int_hash(hash, ptr_hash(prefix->raw_identity()));
    if (namespace_.has_value())
        hash = pair_int_hash(hash, ptr_hash(namespace_->raw_identity()));
    return hash;
}

struct ImplTraits : public Traits<QualifiedName::Impl*> {
    static unsigned hash(QualifiedName::Impl* impl)
    {
        return hash_impl(impl->local_name, impl->prefix, impl->namespace_);
    }

    static bool equals(QualifiedName::Impl* a, QualifiedName::Impl* b)
    {
        return a->local_name == b->local_name
            && a->prefix == b->prefix
            && a->namespace_ == b->namespace_;
    }
};

static constexpr size_t qualified_name_cache_size = 256;
static QualifiedName::Impl* s_qualified_name_cache[qualified_name_cache_size] {};

static HashTable<QualifiedName::Impl*, ImplTraits>& impls()
{
    static NeverDestroyed<HashTable<QualifiedName::Impl*, ImplTraits>> impls;
    return *impls;
}

static constexpr uintptr_t qualified_name_immortal_bit = 1;

static uintptr_t ensure_impl_raw(Utf16FlyString const& local_name, Optional<Utf16FlyString> const& prefix, Optional<Utf16FlyString> const& namespace_, bool make_immortal)
{
    unsigned hash = hash_impl(local_name, prefix, namespace_);
    auto& slot = s_qualified_name_cache[hash & (qualified_name_cache_size - 1)];
    QualifiedName::Impl* found = nullptr;
    if (slot && slot->local_name == local_name && slot->prefix == prefix && slot->namespace_ == namespace_) {
        found = slot;
    } else {
        auto it = impls().find(hash, [&](QualifiedName::Impl* entry) {
            return entry->local_name == local_name
                && entry->prefix == prefix
                && entry->namespace_ == namespace_;
        });
        if (it != impls().end()) {
            found = *it;
            slot = found;
        }
    }

    if (found) {
        if (found->is_immortal)
            return reinterpret_cast<uintptr_t>(found) | qualified_name_immortal_bit;
        if (make_immortal) {
            // Keep a permanent reference so any existing non-immortal handles can unref safely without destroying `found`.
            found->ref();
            found->is_immortal = true;
            return reinterpret_cast<uintptr_t>(found) | qualified_name_immortal_bit;
        }
        found->ref();
        return reinterpret_cast<uintptr_t>(found);
    }

    auto* impl = new QualifiedName::Impl(local_name, prefix, namespace_);
    slot = impl;
    if (make_immortal) {
        impl->is_immortal = true;
        return reinterpret_cast<uintptr_t>(impl) | qualified_name_immortal_bit;
    }
    return reinterpret_cast<uintptr_t>(impl);
}

QualifiedName::QualifiedName(Utf16FlyString const& local_name, Optional<Utf16FlyString> const& prefix, Optional<Utf16FlyString> const& namespace_)
    : m_raw(ensure_impl_raw(local_name, prefix, namespace_, false))
{
}

QualifiedName QualifiedName::make_immortal(Utf16FlyString const& local_name, Optional<Utf16FlyString> const& prefix, Optional<Utf16FlyString> const& namespace_)
{
    return QualifiedName { ensure_impl_raw(local_name, prefix, namespace_, true) };
}

QualifiedName::Impl::Impl(Utf16FlyString const& a_local_name, Optional<Utf16FlyString> const& a_prefix, Optional<Utf16FlyString> const& a_namespace)
    : local_name(a_local_name)
    , lowercased_local_name(local_name.to_ascii_lowercase())
    , prefix(a_prefix)
    , namespace_(a_namespace)
{
    impls().set(this);
    make_internal_string();
}

QualifiedName::Impl::~Impl()
{
    unsigned hash = hash_impl(local_name, prefix, namespace_);
    auto& slot = s_qualified_name_cache[hash & (qualified_name_cache_size - 1)];
    if (slot == this)
        slot = nullptr;
    impls().remove(this);
}

// https://dom.spec.whatwg.org/#concept-attribute-qualified-name
// https://dom.spec.whatwg.org/#concept-element-qualified-name
void QualifiedName::Impl::make_internal_string()
{
    // This is possible to do according to the spec: "User agents could have this as an internal slot as an optimization."
    if (!prefix.has_value()) {
        as_string = local_name;
        return;
    }

    Utf16StringBuilder builder;
    builder.append(prefix->view());
    builder.append_ascii(':');
    builder.append(local_name.view());
    as_string = builder.to_string();
}

Utf16FlyString const& QualifiedName::ascii_uppercased_as_string() const
{
    auto* current = impl();
    return current->ascii_uppercased_as_string.ensure([&] { return current->as_string.to_ascii_uppercase(); });
}

void QualifiedName::set_prefix(Optional<Utf16FlyString> value)
{
    auto* current = impl();
    auto new_raw = ensure_impl_raw(current->local_name, move(value), current->namespace_, false);
    auto old_raw = m_raw;
    m_raw = new_raw;
    if ((old_raw & IMMORTAL_BIT) == 0 && old_raw != 0)
        reinterpret_cast<Impl*>(old_raw)->unref();
}

}
