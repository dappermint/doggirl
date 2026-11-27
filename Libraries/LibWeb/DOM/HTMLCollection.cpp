/*
 * Copyright (c) 2021-2022, Andreas Kling <andreas@ladybird.org>
 * Copyright (c) 2021, Luke Wilde <lukew@serenityos.org>
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

#include <AK/InsertionSort.h>
#include <LibGC/Heap.h>
#include <LibGC/WeakInlines.h>
#include <LibJS/Runtime/ExternalMemory.h>
#include <LibWeb/Bindings/Wrappable.h>
#include <LibWeb/DOM/Document.h>
#include <LibWeb/DOM/Element.h>
#include <LibWeb/DOM/HTMLCollection.h>
#include <LibWeb/DOM/ParentNode.h>
#include <LibWeb/Namespace.h>

namespace Web::DOM {

static constexpr size_t MAX_RETAINED_CACHED_ELEMENTS_CAPACITY = 256;

GC_DEFINE_ALLOCATOR(HTMLCollection);

GC::Ref<HTMLCollection> HTMLCollection::create(ParentNode& root, Scope scope, Filter filter, AttributeInvalidationType attribute_invalidation_type, Sort sort, Kind kind)
{
    return GC::Heap::the().allocate<HTMLCollection>(root, scope, filter, attribute_invalidation_type, sort, kind, MatcherKind::Custom);
}

GC::Ref<HTMLCollection> HTMLCollection::create_all(ParentNode& root, Scope scope)
{
    return GC::Heap::the().allocate<HTMLCollection>(root, scope, nullptr, AttributeInvalidationType::None, nullptr, Kind::Generic, MatcherKind::All);
}

GC::Ref<HTMLCollection> HTMLCollection::create_by_tag_name_html(ParentNode& root, Utf16FlyString qualified_name, Utf16FlyString lowercase_qualified_name)
{
    if (qualified_name == lowercase_qualified_name)
        return create_by_tag_name_exact(root, move(qualified_name));
    auto collection = GC::Heap::the().allocate<HTMLCollection>(root, Scope::Descendants, nullptr, AttributeInvalidationType::None, nullptr, Kind::Generic, MatcherKind::TagNameHTML);
    collection->m_primary_name = move(qualified_name);
    collection->m_secondary_name = move(lowercase_qualified_name);
    return collection;
}

GC::Ref<HTMLCollection> HTMLCollection::create_by_tag_name_exact(ParentNode& root, Utf16FlyString qualified_name)
{
    auto collection = GC::Heap::the().allocate<HTMLCollection>(root, Scope::Descendants, nullptr, AttributeInvalidationType::None, nullptr, Kind::Generic, MatcherKind::TagNameExact);
    collection->m_primary_name = move(qualified_name);
    return collection;
}

GC::Ref<HTMLCollection> HTMLCollection::create_by_class_names(ParentNode& root, Vector<Utf16FlyString> class_names, bool case_insensitive)
{
    if (class_names.is_empty())
        return GC::Heap::the().allocate<HTMLCollection>(root, Scope::Descendants, nullptr, AttributeInvalidationType::None, nullptr, Kind::Generic, MatcherKind::Empty);

    if (class_names.size() == 1) {
        auto collection = GC::Heap::the().allocate<HTMLCollection>(root, Scope::Descendants, nullptr, AttributeInvalidationType::Class, nullptr, Kind::Generic, MatcherKind::SingleClass);
        collection->m_primary_name = move(class_names[0]);
        collection->m_name_filter_mask = case_insensitive ? 0 : Element::name_filter_bit(collection->m_primary_name);
        collection->m_case_insensitive_classes = case_insensitive;
        return collection;
    }

    auto collection = GC::Heap::the().allocate<HTMLCollection>(root, Scope::Descendants, nullptr, AttributeInvalidationType::Class, nullptr, Kind::Generic, MatcherKind::ClassList);
    u64 mask = 0;
    if (!case_insensitive) {
        for (auto const& class_name : class_names)
            mask |= Element::name_filter_bit(class_name);
    }
    collection->m_class_names = move(class_names);
    collection->m_name_filter_mask = mask;
    collection->m_case_insensitive_classes = case_insensitive;
    return collection;
}

HTMLCollection::HTMLCollection(ParentNode& root, Scope scope, GC::Ptr<GC::Function<bool(Element const&)>> filter, AttributeInvalidationType attribute_invalidation_type, Sort sort, Kind kind, MatcherKind matcher_kind)
    : GC::WeakContainer(heap())
    , m_root(root)
    , m_filter(filter)
    , m_sort(sort)
    , m_scope(scope)
    , m_kind(kind)
    , m_attribute_invalidation_type(attribute_invalidation_type)
    , m_matcher_kind(matcher_kind)
{
}

HTMLCollection::~HTMLCollection() = default;

void HTMLCollection::finalize()
{
    Base::finalize();
    invalidate_cache();
    m_cached_elements.clear();
}

void HTMLCollection::visit_edges(GC::Cell::Visitor& visitor)
{
    Base::visit_edges(visitor);
    visitor.visit(m_root);
    visitor.visit(m_filter);
    visitor.visit(m_sort);
}

size_t HTMLCollection::external_memory_size() const
{
    auto size = Base::external_memory_size();
    size = JS::saturating_add_external_memory_size(size, m_cached_elements.capacity() * sizeof(Element*));
    size = JS::saturating_add_external_memory_size(size, JS::vector_external_memory_size(m_class_names));
    return size;
}

bool HTMLCollection::element_matches_filter(Element const& element) const
{
    switch (m_matcher_kind) {
    case MatcherKind::All:
        return true;
    case MatcherKind::Empty:
        return false;
    case MatcherKind::TagNameHTML:
        if (element.namespace_uri() == Namespace::HTML)
            return element.qualified_name() == m_secondary_name;
        return element.qualified_name() == m_primary_name;
    case MatcherKind::TagNameExact:
        return element.qualified_name() == m_primary_name;
    case MatcherKind::SingleClass:
        return element.has_class(m_primary_name, m_case_insensitive_classes ? CaseSensitivity::CaseInsensitive : CaseSensitivity::CaseSensitive);
    case MatcherKind::ClassList: {
        auto sensitivity = m_case_insensitive_classes ? CaseSensitivity::CaseInsensitive : CaseSensitivity::CaseSensitive;
        for (auto const& name : m_class_names) {
            if (!element.has_class(name, sensitivity))
                return false;
        }
        return true;
    }
    case MatcherKind::Custom:
        return (*m_filter)(element);
    }
    VERIFY_NOT_REACHED();
}

GC::Cell const& HTMLCollection::owner_cell(Badge<GC::Heap>) const
{
    return *this;
}

void HTMLCollection::remove_dead_cells(Badge<GC::Heap>)
{
    m_cached_elements.remove_dead_cells(heap());
    if (m_cached_name_to_element_mappings) {
        m_cached_name_to_element_mappings->remove_all_matching([](Utf16FlyString const&, GC::Weak<Element> const& element) {
            return !element;
        });
    }
}

void HTMLCollection::update_name_to_element_mappings_if_needed() const
{
    update_cache_if_needed();
    if (m_cached_name_to_element_mappings)
        return;
    m_cached_name_to_element_mappings = make<OrderedHashMap<Utf16FlyString, GC::Weak<Element>>>();
    for (auto& element : m_cached_elements) {
        // 1. If element has an ID which is not in result, append element’s ID to result.
        if (auto const& id = element.id(); id.has_value()) {
            if (!id.value().is_empty() && !m_cached_name_to_element_mappings->contains(id.value()))
                m_cached_name_to_element_mappings->set(id.value(), element);
        }

        // 2. If element is in the HTML namespace and has a name attribute whose value is neither the empty string nor is in result, append element’s name attribute value to result.
        if (element.namespace_uri() == Namespace::HTML && element.name().has_value()) {
            auto element_name = element.name().value();
            if (!element_name.is_empty() && !m_cached_name_to_element_mappings->contains(element_name))
                m_cached_name_to_element_mappings->set(move(element_name), element);
        }
    }
    m_cached_document->register_valid_html_collection_cache(AttributeInvalidationType::IdOrName);
}

template<typename AppendFn>
void HTMLCollection::collect_elements_with(AppendFn&& append) const
{
    if (m_matcher_kind == MatcherKind::Empty)
        return;

    if (m_scope == Scope::Children) {
        if (m_matcher_kind == MatcherKind::All) {
            m_root->for_each_child_of_type<Element>([&](auto& element) {
                append(element);
                return IterationDecision::Continue;
            });
            return;
        }
        m_root->for_each_child_of_type<Element>([&](auto& element) {
            if (element_matches_filter(element))
                append(element);
            return IterationDecision::Continue;
        });
        return;
    }

    switch (m_matcher_kind) {
    case MatcherKind::All:
        m_root->for_each_in_subtree_of_type<Element>([&](auto& element) {
            append(element);
            return TraversalDecision::Continue;
        });
        return;
    case MatcherKind::TagNameExact: {
        auto const expected_identity = m_primary_name.raw_identity();
        m_root->for_each_in_subtree_of_type<Element>([&](auto& element) {
            if (element.qualified_name().raw_identity() == expected_identity)
                append(element);
            return TraversalDecision::Continue;
        });
        return;
    }
    case MatcherKind::TagNameHTML:
        m_root->for_each_in_subtree_of_type<Element>([&](auto& element) {
            auto const& expected = element.namespace_uri() == Namespace::HTML ? m_secondary_name : m_primary_name;
            if (element.qualified_name() == expected)
                append(element);
            return TraversalDecision::Continue;
        });
        return;
    case MatcherKind::SingleClass:
        if (!m_case_insensitive_classes) {
            u64 const mask = m_name_filter_mask;
            if (auto const* root_element = as_if<Element>(*m_root)) {
                if ((root_element->subtree_name_filter() & mask) != mask)
                    return;
            } else if (auto const* document = as_if<Document>(*m_root)) {
                auto const* doc_element = document->document_element();
                if (!doc_element || (doc_element->subtree_name_filter() & mask) != mask)
                    return;
            }
            m_root->for_each_in_subtree_of_type<Element>([&](auto& element) {
                if ((element.subtree_name_filter() & mask) != mask)
                    return TraversalDecision::SkipChildrenAndContinue;
                if (element.has_class(m_primary_name, CaseSensitivity::CaseSensitive))
                    append(element);
                return TraversalDecision::Continue;
            });
            return;
        }
        m_root->for_each_in_subtree_of_type<Element>([&](auto& element) {
            if (element.has_class(m_primary_name, CaseSensitivity::CaseInsensitive))
                append(element);
            return TraversalDecision::Continue;
        });
        return;
    case MatcherKind::ClassList:
        if (!m_case_insensitive_classes) {
            u64 const mask = m_name_filter_mask;
            if (auto const* root_element = as_if<Element>(*m_root)) {
                if ((root_element->subtree_name_filter() & mask) != mask)
                    return;
            } else if (auto const* document = as_if<Document>(*m_root)) {
                auto const* doc_element = document->document_element();
                if (!doc_element || (doc_element->subtree_name_filter() & mask) != mask)
                    return;
            }
            m_root->for_each_in_subtree_of_type<Element>([&](auto& element) {
                if ((element.subtree_name_filter() & mask) != mask)
                    return TraversalDecision::SkipChildrenAndContinue;
                for (auto const& name : m_class_names) {
                    if (!element.has_class(name, CaseSensitivity::CaseSensitive))
                        return TraversalDecision::Continue;
                }
                append(element);
                return TraversalDecision::Continue;
            });
            return;
        }
        m_root->for_each_in_subtree_of_type<Element>([&](auto& element) {
            for (auto const& name : m_class_names) {
                if (!element.has_class(name, CaseSensitivity::CaseInsensitive))
                    return TraversalDecision::Continue;
            }
            append(element);
            return TraversalDecision::Continue;
        });
        return;
    case MatcherKind::Custom:
        m_root->for_each_in_subtree_of_type<Element>([&](auto& element) {
            if ((*m_filter)(element))
                append(element);
            return TraversalDecision::Continue;
        });
        return;
    case MatcherKind::Empty:
        return;
    }
}

void HTMLCollection::collect_elements_into(GC::RootVector<GC::Ref<Element>>& elements) const
{
    collect_elements_with([&](Element& element) {
        elements.append(element);
    });
}

void HTMLCollection::collect_elements_into_weak_vector(GC::WeakVector<Element>& elements) const
{
    collect_elements_with([&](Element& element) {
        elements.append(element);
    });
}

bool HTMLCollection::cache_is_valid() const
{
    auto& document = root()->document();
    return list_node.is_in_list()
        && m_cached_document.ptr().ptr() == &document
        && m_cached_invalidation_version == invalidation_version(document);
}

void HTMLCollection::update_cache_if_needed() const
{
    if (cache_is_valid())
        return;
    auto& document = root()->document();
    auto invalidation_version = this->invalidation_version(document);

    invalidate_cache();

    m_cached_name_to_element_mappings = nullptr;
    if (m_sort) {
        GC::RootVector<GC::Ref<Element>> elements;
        if (m_last_cached_size > 0)
            elements.ensure_capacity(m_last_cached_size);
        collect_elements_into(elements);
        insertion_sort(elements, [this](auto const& a, auto const& b) {
            return (*this->m_sort)(*a, *b);
        });
        m_cached_elements = GC::WeakVector<Element> { elements };
    } else {
        m_cached_elements.clear_with_capacity();
        if (m_last_cached_size > m_cached_elements.capacity())
            m_cached_elements.ensure_capacity(m_last_cached_size);
        collect_elements_into_weak_vector(m_cached_elements);
    }
    m_last_cached_size = static_cast<u32>(min<size_t>(m_cached_elements.size(), NumericLimits<u32>::max()));

    m_cached_document = document;
    m_cached_invalidation_version = invalidation_version;
    document.register_valid_html_collection_cache(m_attribute_invalidation_type);
    m_root->register_html_collection_with_valid_cache(const_cast<HTMLCollection&>(*this));
}

void HTMLCollection::invalidate_cache() const
{
    if (!list_node.is_in_list())
        return;
    invalidate_name_to_element_mappings();
    if (auto cached_document = m_cached_document.ptr())
        cached_document->unregister_valid_html_collection_cache(m_attribute_invalidation_type);
    list_node.remove();
    if (m_cached_elements.capacity() <= MAX_RETAINED_CACHED_ELEMENTS_CAPACITY)
        m_cached_elements.clear_with_capacity();
    else
        m_cached_elements.clear();
}

void HTMLCollection::invalidate_name_to_element_mappings() const
{
    if (!m_cached_name_to_element_mappings)
        return;
    if (auto cached_document = m_cached_document.ptr())
        cached_document->unregister_valid_html_collection_cache(AttributeInvalidationType::IdOrName);
    m_cached_name_to_element_mappings = nullptr;
}

void HTMLCollection::invalidate_cache_for_tree_mutation(Node const& mutation_parent) const
{
    if (m_scope == Scope::Children && m_root.ptr() != &mutation_parent)
        return;
    invalidate_cache();
}

void HTMLCollection::invalidate_cache_for_attribute_change(Element const& element, AttributeInvalidationTypes invalidation_types) const
{
    if (m_root.ptr() == &element)
        return;
    if (m_scope == Scope::Children && m_root.ptr() != element.parent())
        return;

    auto membership_invalidation_type = HTMLCollectionCacheRegistration::attribute_invalidation_type_mask(m_attribute_invalidation_type);
    if (!(invalidation_types & membership_invalidation_type)) {
        if (invalidation_types & HTMLCollectionCacheRegistration::attribute_invalidation_type_mask(AttributeInvalidationType::IdOrName))
            invalidate_name_to_element_mappings();
        return;
    }

    if (m_sort) {
        invalidate_cache();
        return;
    }

    // OPTIMIZATION: The cache still records whether the element matched before the attribute
    // change. Keep the element vector when reevaluating the filter produces the same result.
    bool was_matching = m_cached_elements.contains(element);
    if (was_matching != element_matches_filter(element)) {
        invalidate_cache();
        return;
    }

    if (invalidation_types & HTMLCollectionCacheRegistration::attribute_invalidation_type_mask(AttributeInvalidationType::IdOrName))
        invalidate_name_to_element_mappings();
}

u64 HTMLCollection::invalidation_version(Document const& document) const
{
    switch (m_kind) {
    case Kind::Generic:
        return 0;
    case Kind::FormControls:
        return document.form_controls_version();
    case Kind::SelectedOptions:
        return document.option_selectedness_version();
    }
    VERIFY_NOT_REACHED();
}

GC::RootVector<GC::Ref<Element>> HTMLCollection::collect_matching_elements() const
{
    update_cache_if_needed();
    GC::RootVector<GC::Ref<Element>> elements;
    for (auto& element : m_cached_elements)
        elements.append(element);
    return elements;
}

// https://dom.spec.whatwg.org/#dom-htmlcollection-length
size_t HTMLCollection::length() const
{
    // The length getter steps are to return the number of nodes represented by the collection.
    update_cache_if_needed();
    return m_cached_elements.size();
}

// https://dom.spec.whatwg.org/#dom-htmlcollection-item
Element* HTMLCollection::item(size_t index) const
{
    // The item(index) method steps are to return the indexth element in the collection. If there is no indexth element in the collection, then the method must return null.
    update_cache_if_needed();
    if (index >= m_cached_elements.size())
        return nullptr;
    return &m_cached_elements[index];
}

// https://dom.spec.whatwg.org/#dom-htmlcollection-nameditem-key
Element* HTMLCollection::named_item(Utf16View key) const
{
    // 1. If key is the empty string, return null.
    if (key.is_empty())
        return nullptr;

    update_name_to_element_mappings_if_needed();
    if (auto it = m_cached_name_to_element_mappings->get(key); it.has_value())
        return it.value().ptr().ptr();
    return nullptr;
}

// https://dom.spec.whatwg.org/#ref-for-dfn-supported-property-names
bool HTMLCollection::is_supported_property_name(Utf16FlyString const& name) const
{
    update_name_to_element_mappings_if_needed();
    return m_cached_name_to_element_mappings->contains(name);
}

// https://dom.spec.whatwg.org/#ref-for-dfn-supported-property-names
Vector<Utf16FlyString> HTMLCollection::supported_property_names() const
{
    // 1. Let result be an empty list.
    Vector<Utf16FlyString> result;

    // 2. For each element represented by the collection, in tree order:
    update_name_to_element_mappings_if_needed();
    for (auto const& it : *m_cached_name_to_element_mappings) {
        result.append(it.key);
    }

    // 3. Return result.
    return result;
}

}
