/*
 * Copyright (c) 2025, Tim Flynn <trflynn89@ladybird.org>
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

#include <AK/HashTable.h>
#include <AK/Mutex.h>
#include <AK/Singleton.h>
#include <AK/Utf16FlyString.h>
#include <AK/kmalloc.h>

namespace AK {

struct Utf16FlyStringTableHashTraits : public Traits<Detail::Utf16StringData const*> {
    static u32 hash(Detail::Utf16StringData const* string) { return string->hash(); }
    static bool equals(Detail::Utf16StringData const* a, Detail::Utf16StringData const* b) { return *a == *b; }
    static constexpr bool may_have_slow_equality_check() { return true; }
};

struct Utf16FlyStringShard {
    // Interned data only removes itself once its destructor holds this mutex, so lookups must try_ref() entries, and
    // nothing may drop a string reference while holding it.
    Mutex mutex;
    HashTable<Detail::Utf16StringData const*, Utf16FlyStringTableHashTraits> strings;
};

#if defined(AK_OS_MACOS) && defined(AK_ARCH_AARCH64)
struct alignas(128) AlignedUtf16FlyStringShard : public Utf16FlyStringShard { };
#else
struct alignas(64) AlignedUtf16FlyStringShard : public Utf16FlyStringShard { };
#endif

struct Utf16FlyStringTable {
    AK_ALLOC_WITH_KMALLOC;

    static constexpr size_t shard_count = 64;

    static constexpr size_t shard_index(u32 hash)
    {
        return (hash >> 24) & (shard_count - 1);
    }

    Utf16FlyStringShard& shard_for(u32 hash) { return shards[shard_index(hash)]; }

    AlignedUtf16FlyStringShard shards[shard_count];
};

static Utf16FlyStringTable& utf16_fly_string_table()
{
    static Singleton<Utf16FlyStringTable> table;
    return *table;
}

namespace Detail {

void did_destroy_utf16_fly_string_data(Badge<Detail::Utf16StringData>, Detail::Utf16StringData const& data)
{
    auto hash = data.hash();
    auto& shard = utf16_fly_string_table().shard_for(hash);
    MutexLocker locker { shard.mutex };

    // An equal string may have replaced this entry while the destructor waited for the mutex.
    auto it = shard.strings.find(hash, [&](auto const& entry) { return entry == &data; });
    if (it != shard.strings.end())
        shard.strings.remove(it);
}

}

template<typename ViewType>
Optional<Utf16FlyString> Utf16FlyString::create_fly_string_from_cache(ViewType const& string)
{
    if (string.is_empty())
        return {};

    if constexpr (IsSame<ViewType, StringView>) {
        if (string.length() <= Detail::MAX_SHORT_STRING_BYTE_COUNT && string.is_ascii())
            return Utf16String::from_utf8_without_validation(string);
    } else {
        if (string.length_in_code_units() <= Detail::MAX_SHORT_STRING_BYTE_COUNT && string.is_ascii())
            return Utf16String::from_utf16(string);
    }

    auto hash = string.hash();
    auto& shard = utf16_fly_string_table().shard_for(hash);
    MutexLocker locker { shard.mutex };

    auto it = shard.strings.find(hash, [&](auto const& entry) { return *entry == string; });
    if (it != shard.strings.end() && (*it)->try_ref())
        return Utf16FlyString { Detail::Utf16StringBase(adopt_ref(**it)) };

    return {};
}

Utf16FlyString Utf16FlyString::from_utf8(StringView string)
{
    if (auto result = create_fly_string_from_cache(string); result.has_value())
        return result.release_value();
    return Utf16String::from_utf8(string);
}

Utf16FlyString Utf16FlyString::from_ascii_without_validation(StringView string)
{
    if (auto result = create_fly_string_from_cache(string); result.has_value())
        return result.release_value();
    return Utf16String::from_ascii_without_validation(string.bytes());
}

Utf16FlyString Utf16FlyString::from_utf8_without_validation(StringView string)
{
    if (auto result = create_fly_string_from_cache(string); result.has_value())
        return result.release_value();
    return Utf16String::from_utf8_without_validation(string);
}

Utf16FlyString Utf16FlyString::from_utf16(Utf16View const& string)
{
    if (auto result = create_fly_string_from_cache(string); result.has_value())
        return result.release_value();
    return Utf16String::from_utf16(string);
}

Utf16FlyString::Utf16FlyString(Utf16String const& string)
{
    if (string.has_short_ascii_storage()) {
        m_data = string;
        return;
    }

    auto const* data = string.data({});

    if (data->is_fly_string()) {
        m_data = string;
        return;
    }

    auto hash = data->hash();
    auto& shard = utf16_fly_string_table().shard_for(hash);
    MutexLocker locker { shard.mutex };

    if (auto it = shard.strings.find(hash, [&](auto const& entry) { return *entry == *data; }); it != shard.strings.end() && (*it)->try_ref()) {
        m_data = Detail::Utf16StringBase(adopt_ref(**it));
        return;
    }

    // Replaces any equal entry that is waiting to be removed by its destructor.
    m_data = string;
    shard.strings.set(data);
    data->mark_as_fly_string({});
}

size_t Utf16FlyString::number_of_utf16_fly_strings()
{
    auto& table = utf16_fly_string_table();
    size_t count = 0;
    for (auto& shard : table.shards) {
        MutexLocker locker { shard.mutex };
        count += shard.strings.size();
    }
    return count;
}

}
