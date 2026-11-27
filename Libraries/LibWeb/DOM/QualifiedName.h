/*
 * Copyright (c) 2020, the SerenityOS developers.
 * Copyright (c) 2022, Andreas Kling <andreas@ladybird.org>
 * Copyright (c) 2023, Shannon Booth <shannon@serenityos.org>
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

#pragma once

#include <AK/Optional.h>
#include <AK/RefCounted.h>
#include <AK/Utf16FlyString.h>
#include <LibWeb/Export.h>

namespace Web::DOM {

class WEB_API QualifiedName {
public:
    QualifiedName(Utf16FlyString const& local_name, Optional<Utf16FlyString> const& prefix, Optional<Utf16FlyString> const& namespace_);
    static QualifiedName make_immortal(Utf16FlyString const& local_name, Optional<Utf16FlyString> const& prefix, Optional<Utf16FlyString> const& namespace_);

    ALWAYS_INLINE QualifiedName(QualifiedName const& other)
        : m_raw(other.m_raw)
    {
        if ((m_raw & IMMORTAL_BIT) == 0 && m_raw != 0)
            impl()->ref();
    }

    ALWAYS_INLINE QualifiedName(QualifiedName&& other) noexcept
        : m_raw(other.m_raw)
    {
        if ((other.m_raw & IMMORTAL_BIT) == 0)
            other.m_raw = 0;
    }

    ALWAYS_INLINE ~QualifiedName()
    {
        if ((m_raw & IMMORTAL_BIT) == 0 && m_raw != 0)
            impl()->unref();
    }

    ALWAYS_INLINE QualifiedName& operator=(QualifiedName const& other)
    {
        if (this != &other) {
            if ((other.m_raw & IMMORTAL_BIT) == 0 && other.m_raw != 0)
                other.impl()->ref();
            auto old_raw = m_raw;
            m_raw = other.m_raw;
            if ((old_raw & IMMORTAL_BIT) == 0 && old_raw != 0)
                reinterpret_cast<Impl*>(old_raw)->unref();
        }
        return *this;
    }

    ALWAYS_INLINE QualifiedName& operator=(QualifiedName&& other) noexcept
    {
        if (this != &other) {
            auto old_raw = m_raw;
            m_raw = other.m_raw;
            if ((other.m_raw & IMMORTAL_BIT) == 0)
                other.m_raw = 0;
            if ((old_raw & IMMORTAL_BIT) == 0 && old_raw != 0)
                reinterpret_cast<Impl*>(old_raw)->unref();
        }
        return *this;
    }

    Utf16FlyString const& local_name() const { return impl()->local_name; }
    Optional<Utf16FlyString> const& prefix() const { return impl()->prefix; }
    Optional<Utf16FlyString> const& namespace_() const { return impl()->namespace_; }

    Utf16FlyString const& lowercased_local_name() const { return impl()->lowercased_local_name; }

    Utf16FlyString const& as_string() const { return impl()->as_string; }
    Utf16FlyString const& ascii_uppercased_as_string() const;

    struct WEB_API Impl : public RefCounted<Impl> {
        Impl(Utf16FlyString const& local_name, Optional<Utf16FlyString> const& prefix, Optional<Utf16FlyString> const& namespace_);
        ~Impl();

        void make_internal_string();
        Utf16FlyString local_name;
        Utf16FlyString lowercased_local_name;
        Optional<Utf16FlyString> prefix;
        Optional<Utf16FlyString> namespace_;
        Utf16FlyString as_string;
        mutable Optional<Utf16FlyString> ascii_uppercased_as_string;
        bool is_immortal { false };
    };

    // Names that are equal in every part share their Impl, so this tells them apart in one comparison.
    Impl const* identity() const { return impl(); }

    void set_prefix(Optional<Utf16FlyString> value);

private:
    static constexpr uintptr_t IMMORTAL_BIT = 1;
    static_assert(alignof(Impl) >= 2);

    explicit constexpr QualifiedName(uintptr_t raw)
        : m_raw(raw)
    {
    }

    ALWAYS_INLINE Impl* impl() const { return reinterpret_cast<Impl*>(m_raw & ~IMMORTAL_BIT); }

    uintptr_t m_raw { 0 };
};

}
