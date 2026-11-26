/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

#include <LibUnicode/ICU.h>
#include <LibUnicode/TextMapping.h>
#include <string.h>
#include <unicode/brkiter.h>
#include <unicode/casemap.h>
#include <unicode/edits.h>
#include <unicode/locid.h>
#include <unicode/stringoptions.h>

namespace Unicode {

static icu::BreakIterator* cached_title_break_iterator(char const* locale_name)
{
    struct Cache {
        bool initialized { false };
        bool is_root { false };
        size_t locale_length { 0 };
        char locale_name[64] {};
        OwnPtr<icu::BreakIterator> iterator;
    };
#ifdef AK_COMPILER_CLANG
    AK_IGNORE_DIAGNOSTIC("-Wexit-time-destructors", static thread_local Cache cache {})
#else
    static thread_local Cache cache {};
#endif

    bool const is_root = locale_name != nullptr && locale_name[0] == '\0';
    char const* effective_name = locale_name ? locale_name : icu::Locale::getDefault().getName();
    size_t const effective_length = is_root ? 0 : strlen(effective_name);
    if (cache.initialized
        && cache.is_root == is_root
        && (is_root || (effective_length < sizeof(cache.locale_name) && cache.locale_length == effective_length && memcmp(cache.locale_name, effective_name, effective_length) == 0))) {
        return cache.iterator.ptr();
    }

    UErrorCode status = U_ZERO_ERROR;
    icu::Locale locale = is_root
        ? icu::Locale::getRoot()
        : (locale_name ? icu::Locale(locale_name) : icu::Locale::getDefault());
    auto iterator = adopt_own_if_nonnull(icu::BreakIterator::createWordInstance(locale, status));
    if (U_FAILURE(status) || !iterator)
        return nullptr;

    cache.iterator = move(iterator);
    cache.is_root = is_root;
    if (!is_root && effective_length < sizeof(cache.locale_name)) {
        memcpy(cache.locale_name, effective_name, effective_length);
        cache.locale_length = effective_length;
        cache.initialized = true;
    } else if (is_root) {
        cache.locale_length = 0;
        cache.initialized = true;
    } else {
        cache.initialized = false;
    }
    return cache.iterator.ptr();
}

static void apply_case_mapping_to(Utf16View const& string, CaseMapping mapping, Optional<Utf16View> const& locale, bool preserve_existing, UnicodeTextMappingOutput output)
{
    Optional<icu::UnicodeString> owned_source;
    char16_t const* source = nullptr;
    i32 source_length = 0;
    if (!string.has_latin1_storage()) {
        auto span = string.utf16_span();
        source = span.data();
        source_length = static_cast<i32>(span.size());
    } else {
        owned_source.emplace(icu_string(string));
        source = reinterpret_cast<char16_t const*>(owned_source->getBuffer());
        source_length = owned_source->length();
    }

    char const* locale_name = nullptr;
    if (locale.has_value()) {
        auto locale_string = MUST(locale->to_utf8(AllowLonelySurrogates::Yes));
        if (auto locale_data = LocaleData::for_locale(locale_string.bytes_as_string_view()); locale_data.has_value())
            locale_name = locale_data->locale().getName();
    }

    icu::BreakIterator* title_break_iterator = mapping == CaseMapping::Titlecase
        ? cached_title_break_iterator(locale_name)
        : nullptr;

    auto map = [&](char16_t* destination, i32 destination_capacity, icu::Edits* edits, UErrorCode& status) {
        switch (mapping) {
        case CaseMapping::Lowercase:
            return icu::CaseMap::toLower(locale_name, 0, source, source_length, destination, destination_capacity, edits, status);
        case CaseMapping::Uppercase:
            return icu::CaseMap::toUpper(locale_name, 0, source, source_length, destination, destination_capacity, edits, status);
        case CaseMapping::Titlecase: {
            u32 options = 0;
            if (preserve_existing)
                options |= U_TITLECASE_NO_LOWERCASE;
            return icu::CaseMap::toTitle(locale_name, options, title_break_iterator, source, source_length, destination, destination_capacity, edits, status);
        }
        }
        VERIFY_NOT_REACHED();
    };

    constexpr i32 stack_capacity = 256;
    char16_t stack_buffer[stack_capacity];
    icu::Edits icu_edits;
    UErrorCode status = U_ZERO_ERROR;
    auto const destination_length = map(stack_buffer, stack_capacity, &icu_edits, status);
    if (status != U_BUFFER_OVERFLOW_ERROR)
        verify_icu_success(status);
    auto* destination = reinterpret_cast<char16_t*>(output.allocate_text(output.context, destination_length));
    if (status == U_BUFFER_OVERFLOW_ERROR) {
        status = U_ZERO_ERROR;
        VERIFY(map(destination, destination_length, &icu_edits, status) == destination_length);
        verify_icu_success(status);
    } else if (destination_length > 0) {
        memcpy(destination, stack_buffer, static_cast<size_t>(destination_length) * sizeof(char16_t));
    }

    auto iterator = icu_edits.getFineChangesIterator();
    while (iterator.next(status)) {
        if (iterator.oldLength() == iterator.newLength())
            continue;
        output.append_edit(output.context,
            static_cast<size_t>(iterator.sourceIndex()),
            static_cast<size_t>(iterator.oldLength()),
            static_cast<size_t>(iterator.destinationIndex()),
            static_cast<size_t>(iterator.newLength()));
    }
    verify_icu_success(status);
}

}

extern "C" void unicode_apply_case_mapping(u16 const* text, size_t length, u8 mapping, u16 const* locale, size_t locale_length, bool preserve_existing, UnicodeTextMappingOutput output)
{
    VERIFY(mapping <= to_underlying(Unicode::CaseMapping::Titlecase));
    auto locale_view = locale ? Optional<Utf16View> { Utf16View { reinterpret_cast<char16_t const*>(locale), locale_length } } : Optional<Utf16View> {};
    Unicode::apply_case_mapping_to({ reinterpret_cast<char16_t const*>(text), length }, static_cast<Unicode::CaseMapping>(mapping), locale_view,
        preserve_existing, output);
}
