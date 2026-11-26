/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

#include <AK/AnyOf.h>
#include <AK/Array.h>
#include <AK/QuickSort.h>
#include <LibGC/RootVector.h>
#include <LibGC/WeakInlines.h>
#include <LibWeb/CSS/SelectorMatching.h>
#include <LibWeb/DOM/Document.h>
#include <LibWeb/DOM/Element.h>
#include <LibWeb/DOM/ParentNode.h>
#include <LibWeb/DOM/SelectorQuery.h>
#include <LibWeb/DOM/ShadowRoot.h>
#include <LibWeb/DOM/StaticNodeList.h>
#include <LibWeb/HTML/CustomElements/CustomStateSet.h>
#include <LibWeb/HTML/HTMLHeadingElement.h>
#include <LibWeb/Namespace.h>
#include <LibWeb/SelectorRustFFI.h>

namespace Web::DOM {

// Whether matching this pseudo-class can only change when Document::dom_tree_version() or
// Document::character_data_version() change, i.e. it depends only on tree structure, attributes and character
// data. Pseudo-classes that also depend on other state (user interaction, element states like checkedness or
// validity, navigation, history, custom element upgrades, ...) must not be on this list.
static bool pseudo_class_matching_is_covered_by_version_counters(CSS::PseudoClass pseudo_class)
{
    switch (pseudo_class) {
    case CSS::PseudoClass::AnyLink: // Presence of an href attribute.
    case CSS::PseudoClass::Empty:   // Child nodes and their character data.
    case CSS::PseudoClass::FirstChild:
    case CSS::PseudoClass::FirstOfType:
    case CSS::PseudoClass::Has: // Container; argument selectors are checked via the recursive pseudo-class bitmap.
    case CSS::PseudoClass::Heading:
    case CSS::PseudoClass::Is:
    case CSS::PseudoClass::LastChild:
    case CSS::PseudoClass::LastOfType:
    case CSS::PseudoClass::Link: // Matches like :any-link; we treat all links as unvisited.
    case CSS::PseudoClass::Not:
    case CSS::PseudoClass::NthChild:
    case CSS::PseudoClass::NthLastChild:
    case CSS::PseudoClass::NthLastOfType:
    case CSS::PseudoClass::NthOfType:
    case CSS::PseudoClass::OnlyChild:
    case CSS::PseudoClass::OnlyOfType:
    case CSS::PseudoClass::Optional:
    case CSS::PseudoClass::Required:
    case CSS::PseudoClass::Root:
    case CSS::PseudoClass::Scope: // The scoping root is part of the result cache key.
    case CSS::PseudoClass::Where:
        return true;
    default:
        return false;
    }
}

SelectorQuery::SelectorQuery(CSS::SelectorList&& selectors)
    : m_selectors(move(selectors))
{
    auto filter_bits_of = [](FastCompound const& compound) -> u64 {
        u64 bits = 0;
        for (u8 i = 0; i < compound.class_count; ++i)
            bits |= Element::name_filter_bit_from_raw(compound.classes[i]);
        if (compound.attribute_name)
            bits |= Element::name_filter_bit_from_raw(compound.attribute_name);
        return bits;
    };
    auto attribute_filter_bits_of = [](FastCompound const& compound) -> u32 {
        return compound.attribute_name ? Element::attribute_filter_bits_from_raw(compound.attribute_name) : 0;
    };
    auto copy_compound = [](CSS::SelectorFFI::FfiSimpleCompound const& src) -> FastCompound {
        return {
            .tag_name = src.tag_name,
            .id = src.id,
            .classes = { src.classes[0], src.classes[1] },
            .attribute_name = src.attribute_name,
            .attribute_value = src.attribute_value,
            .class_count = src.class_count,
            .attribute_match_kind = static_cast<AttributeMatchKind>(src.attribute_match_kind),
        };
    };

    m_is_result_cacheable = true;
    for (auto const& selector : m_selectors) {
        auto const facts = CSS::SelectorFFI::rust_selector_query_facts(&selector->rust_selector());
        if (m_selectors.size() == 1) {
            m_matches_every_element = facts.matches_every_element;
            switch (facts.fast_path.kind) {
            case CSS::SelectorFFI::FfiSelectorFastPathKind::None:
                break;
            case CSS::SelectorFFI::FfiSelectorFastPathKind::SimpleCompound:
                m_fast_path_kind = FastPathKind::SimpleCompound;
                m_fast_subject = copy_compound(facts.fast_path.subject);
                m_subject_filter_bits = filter_bits_of(m_fast_subject);
                m_subject_attribute_filter_bits = attribute_filter_bits_of(m_fast_subject);
                break;
            case CSS::SelectorFFI::FfiSelectorFastPathKind::SimpleDescendantSimpleCompound:
                m_fast_path_kind = FastPathKind::SimpleDescendantSimpleCompound;
                m_fast_subject = copy_compound(facts.fast_path.subject);
                m_fast_ancestor = copy_compound(facts.fast_path.ancestor);
                m_subject_filter_bits = filter_bits_of(m_fast_subject);
                m_ancestor_filter_bits = filter_bits_of(m_fast_ancestor);
                m_subject_attribute_filter_bits = attribute_filter_bits_of(m_fast_subject);
                m_ancestor_attribute_filter_bits = attribute_filter_bits_of(m_fast_ancestor);
                break;
            case CSS::SelectorFFI::FfiSelectorFastPathKind::SimpleChildSimpleCompound:
                m_fast_path_kind = FastPathKind::SimpleChildSimpleCompound;
                m_fast_subject = copy_compound(facts.fast_path.subject);
                m_fast_ancestor = copy_compound(facts.fast_path.ancestor);
                m_subject_filter_bits = filter_bits_of(m_fast_subject);
                m_ancestor_filter_bits = filter_bits_of(m_fast_ancestor);
                m_subject_attribute_filter_bits = attribute_filter_bits_of(m_fast_subject);
                m_ancestor_attribute_filter_bits = attribute_filter_bits_of(m_fast_ancestor);
                break;
            }
        }
        // NB: The contained-pseudo-class bitmap may be under-collected for selectors containing the nesting
        //     selector, so we cannot rely on it (and `&` has no useful meaning in a query anyway).
        if (facts.contains_nesting) {
            m_is_result_cacheable = false;
            break;
        }
        for (size_t i = 0; i < to_underlying(CSS::PseudoClass::__Count); ++i) {
            if (!(facts.pseudo_classes[i / 64] & (1ull << (i % 64))))
                continue;
            auto pseudo_class = static_cast<CSS::PseudoClass>(i);
            if (!pseudo_class_matching_is_covered_by_version_counters(pseudo_class)) {
                m_is_result_cacheable = false;
                break;
            }
            if (pseudo_class == CSS::PseudoClass::Empty)
                m_depends_on_character_data = true;
        }
        if (!m_is_result_cacheable)
            break;
    }
}

SelectorQuery::~SelectorQuery()
{
    CSS::SelectorFFI::rust_dom_selector_program_destroy(m_program);
}

CSS::SelectorFFI::DomSelectorProgram const& SelectorQuery::program(Document const& document) const
{
    // A document is an HTML document or not for all its life, so a query recompiles only if it is used in both kinds.
    if (m_program && m_program_is_for_html_document == document.is_html_document())
        return *m_program;
    CSS::SelectorFFI::rust_dom_selector_program_destroy(m_program);
    Vector<CSS::SelectorFFI::RustSelector const*> selectors;
    selectors.ensure_capacity(m_selectors.size());
    for (auto const& selector : m_selectors)
        selectors.unchecked_append(&selector->rust_selector());
    m_program_is_for_html_document = document.is_html_document();
    m_program = CSS::SelectorFFI::rust_dom_selector_program_create(selectors.span(), m_program_is_for_html_document ? Namespace::HTML.raw_identity() : 0);
    return *m_program;
}

namespace {

#include <LibWeb/SelectorStateFactsGenerated.inc>

// Every node crosses to the DOM matcher as a Node pointer, and an element is downcast from one where the matcher asks
// about an element.
Node const& node_from_ffi(void const* node)
{
    return *static_cast<Node const*>(node);
}

Element const& element_from_ffi(void const* element)
{
    return static_cast<Element const&>(node_from_ffi(element));
}

void const* node_to_ffi(Node const* node)
{
    return node;
}

CSS::SelectorFFI::FfiUtf16View utf16_view_to_ffi(Utf16View view)
{
    return {
        .latin1 = view.has_latin1_storage() ? view.latin1_span().data() : nullptr,
        .utf16 = view.has_latin1_storage() ? nullptr : reinterpret_cast<u16 const*>(view.utf16_span().data()),
        .length = view.length_in_code_units(),
    };
}

static_assert(sizeof(Utf16FlyString) == sizeof(uintptr_t));

constexpr CSS::SelectorFFI::FfiDomSelectorCallbacks dom_selector_callbacks {
    .element = [](void const* pointer, uintptr_t const* names, size_t name_count, CSS::SelectorFFI::DomAttribute* attributes, size_t capacity, CSS::SelectorFFI::FfiDomElement* facts) {
        auto const& element = element_from_ffi(pointer);
        auto const& namespace_uri = element.namespace_uri();
        auto const& classes = element.class_names();
        size_t attribute_count = 0;
        if (name_count > 0) {
            ReadonlySpan<uintptr_t> wanted_names { names, name_count };
            for (auto const& attribute : element.attribute_list()) {
                auto local_name = attribute.name.local_name().raw_identity();
                if (!wanted_names.contains_slow(local_name))
                    continue;
                if (attribute_count < capacity) {
                    auto const& attribute_namespace = attribute.name.namespace_();
                    attributes[attribute_count] = {
                        .local_name = local_name,
                        .namespace_uri = attribute_namespace.has_value() && !attribute_namespace->is_empty() ? attribute_namespace->raw_identity() : 0,
                        .value = utf16_view_to_ffi(attribute.value),
                    };
                }
                ++attribute_count;
            }
        }
        *facts = {
            .local_name = element.local_name().raw_identity(),
            .namespace_uri = namespace_uri.has_value() && !namespace_uri->is_empty() ? namespace_uri->raw_identity() : 0,
            .id = element.id().has_value() ? element.id()->raw_identity() : 0,
            .classes = reinterpret_cast<uintptr_t const*>(classes.data()),
            .class_count = classes.size(),
            .attribute_count = attribute_count,
        }; },
    .same_type = [](void const* element, void const* other) {
        auto const& a = element_from_ffi(element);
        auto const& b = element_from_ffi(other);
        return a.local_name() == b.local_name() && a.namespace_uri() == b.namespace_uri(); },
    .parent = [](void const* pointer) -> void const* {
        // The node is an element or a shadow root, and a shadow root has no parent here.
        auto const& node = node_from_ffi(pointer);
        if (!node.is_element())
            return nullptr;
        auto const* parent = node.parent();
        if (!parent || !(parent->is_element() || is<ShadowRoot>(*parent)))
            return nullptr;
        return node_to_ffi(parent);
    },
    .previous_element_sibling = [](void const* element) { return node_to_ffi(element_from_ffi(element).previous_element_sibling()); },
    .next_element_sibling = [](void const* element) { return node_to_ffi(element_from_ffi(element).next_element_sibling()); },
    .first_element_child = [](void const* node) { return node_to_ffi(node_from_ffi(node).first_child_of_type<Element>()); },
    .first_element_sibling = [](void const* pointer) {
        auto const& node = node_from_ffi(pointer);
        auto const* parent = node.parent();
        return node_to_ffi(parent ? parent->first_child_of_type<Element>() : &node); },
    .child_index = [](void const* element, CSS::SelectorFFI::FfiChildIndex which) {
        using enum CSS::SelectorFFI::FfiChildIndex;
        using enum Element::ChildIndexAmong;
        auto const& subject = element_from_ffi(element);
        switch (which) {
        case FromStart:
            return subject.child_index(Siblings);
        case FromEnd:
            return subject.child_index_from_end(Siblings);
        case OfTypeFromStart:
            return subject.child_index(SiblingsOfType);
        case OfTypeFromEnd:
            return subject.child_index_from_end(SiblingsOfType);
        }
        VERIFY_NOT_REACHED(); },
    .next_element_in_subtree = [](void const* node, void const* root, uintptr_t local_name, uintptr_t class_name, u64 names) -> void const* {
        auto const& stay_within = node_from_ffi(root);
        auto const* next = node_from_ffi(node).next_in_pre_order(&stay_within);
        while (next) {
            auto const* element = as_if<Element>(*next);
            if (element && (element->subtree_name_filter() & names) != names) {
                // No element of this subtree has every name, so skip past it.
                while (next != &stay_within && !next->next_sibling())
                    next = next->parent();
                next = next == &stay_within ? nullptr : next->next_sibling();
                continue;
            }
            if (element && (!local_name || element->local_name().raw_identity() == local_name)
                && (!class_name || any_of(element->class_names(), [&](auto const& name) { return name.raw_identity() == class_name; })))
                return node_to_ffi(element);
            next = next->next_in_pre_order(&stay_within);
        }
        return nullptr;
    },
    .name_filter_bit = [](uintptr_t local_name) { return Element::name_filter_bit_from_raw(local_name); },
    .tree_name_filter = [](void const* node) -> u64 {
        // The filter of a document's one element child holds every name of the document's tree.
        auto const& root = node_from_ffi(node).root();
        if (auto const* document = as_if<Document>(root)) {
            auto const* document_element = document->document_element();
            return document_element ? document_element->subtree_name_filter() : 0;
        }
        if (auto const* element = as_if<Element>(root))
            return element->subtree_name_filter();
        return NumericLimits<u64>::max();
    },
    .shadow_root = [](void const* element) { return node_to_ffi(element_from_ffi(element).shadow_root().ptr()); },
    .host = [](void const* shadow_root) { return node_to_ffi(static_cast<ShadowRoot const&>(node_from_ffi(shadow_root)).host()); },
    .id_or_class_equals_ignoring_ascii_case = [](void const* pointer, bool is_class, uintptr_t name_identity) {
        auto const& element = element_from_ffi(pointer);
        auto name = Utf16FlyString::from_raw(name_identity);
        if (!is_class)
            return element.id().has_value() && element.id()->equals_ignoring_ascii_case(name);
        return any_of(element.class_names(), [&](auto const& class_name) { return class_name.equals_ignoring_ascii_case(name); }); },
    .matches_state = [](void const* element, u8 state) { return SelectorMatching::element_matches_state(element_from_ffi(element), pseudo_classes_by_state_fact[state]); },
    .language = [](void const* element) -> CSS::SelectorFFI::FfiUtf16View {
        auto language = element_from_ffi(element).lang_view();
        if (!language.has_value())
            return {};
        return utf16_view_to_ffi(*language);
    },
    .directionality = [](void const* element) -> size_t {
        auto const& directionality = element_from_ffi(element).directionality() == Element::Directionality::Rtl ? "rtl"_utf16_fly_string : "ltr"_utf16_fly_string;
        return directionality.raw_identity(); },
    .heading_level = [](void const* element) -> u32 {
        auto const* heading = as_if<HTML::HTMLHeadingElement>(element_from_ffi(element));
        return heading ? heading->heading_level() : 0;
    },
    .has_custom_state = [](void const* element, uintptr_t state) {
        auto states = element_from_ffi(element).custom_state_set();
        return states && states->has_state(Utf16FlyString::from_raw(state)); },
    .is_empty = [](void const* pointer) {
        // An element is never its own child, so ignoring itself ignores nothing.
        auto const& element = element_from_ffi(pointer);
        return SelectorMatching::element_is_empty_ignoring_child(element, element); },
    .is_document_element = [](void const* element) {
        auto const* parent = element_from_ffi(element).parent();
        return parent && parent->is_document(); },
};

// A selector query as the DOM matcher takes it, made in the context of the tree `node` is in and scoped to `scope`.
class DomSelectorQuery {
public:
    DomSelectorQuery(CSS::SelectorFFI::DomSelectorProgram const& program, Node const& node, ParentNode const& scope)
    {
        auto const& document = node.document();
        // https://drafts.csswg.org/selectors-4/#scope-pseudo
        // If the :scope elements are not explicitly specified, but the selector is scoped and the scoping root is an
        // element, then :scope represents the scoping root; otherwise, it represents the root of the document
        // (equivalent to :root).
        // FIXME: A query scoped to a DocumentFragment or ShadowRoot has that node as a virtual scoping root, which
        //        :scope should represent instead of the document element.
        auto const* scope_element = as_if<Element>(scope);
        m_query = {
            .program = &program,
            .callbacks = &dom_selector_callbacks,
            .scope = node_to_ffi(scope_element ? scope_element : document.document_element()),
            .shadow_root = node_to_ffi(as_if<ShadowRoot>(node.root())),
            .ids_and_classes_ignore_case = document.in_quirks_mode(),
        };
    }

    bool matches(Element const& element) const
    {
        return CSS::SelectorFFI::rust_dom_selector_query_matches(&m_query, node_to_ffi(&element));
    }

    GC::Ptr<Element const> closest(Element const& element) const
    {
        auto const* found = CSS::SelectorFFI::rust_dom_selector_query_closest(&m_query, node_to_ffi(&element));
        return found ? &element_from_ffi(found) : nullptr;
    }

    GC::Ptr<Element> first(ParentNode& root) const
    {
        GC::Ptr<Element> result;
        for_each_match(root, [&](Element& element) {
            result = element;
            return true;
        });
        return result;
    }

    void collect(ParentNode& root, GC::RootVector<GC::Ref<Node>>& nodes) const
    {
        for_each_match(root, [&](Element& element) {
            nodes.append(element);
            return false;
        });
    }

private:
    // Calls `callback` with each element under `root` that matches the query, in tree order, until it returns true.
    template<typename Callback>
    void for_each_match(ParentNode& root, Callback callback) const
    {
        using enum CSS::SelectorFFI::FfiDomIdLookupKind;
        auto lookup = CSS::SelectorFFI::rust_dom_selector_program_id_lookup(m_query.program);
        // NB: An id the query names is looked up in the id map of the tree the query is made in, which holds the
        //     elements of a connected tree. A quirks-mode document compares ids ignoring case, which the map does not.
        auto const& document = root.document();
        if (lookup.kind == None || !root.is_connected() || document.in_quirks_mode()) {
            for_each_in_subtree(root, callback);
            return;
        }
        auto const& tree_root = root.root();
        auto const* shadow_root = as_if<ShadowRoot>(tree_root);
        auto const& element_by_id = shadow_root ? shadow_root->element_by_id() : document.element_by_id();
        auto elements = element_by_id.elements_with_id(Utf16FlyString::from_raw(lookup.id));
        auto is_under_root = [&](Node const& node) {
            return &tree_root == &root ? &node != &root : node.is_descendant_of(root);
        };

        if (lookup.kind == Subject) {
            // Only the elements with the id can match.
            GC::RootVector<GC::Ref<Element>, 1> candidates;
            for (auto const& element : elements) {
                if (element && is_under_root(*element))
                    candidates.append(*element);
            }
            if (candidates.size() > 1)
                quick_sort(candidates, [](auto const& a, auto const& b) { return a->is_before(*b); });
            for (auto& element : candidates) {
                if (matches(*element) && callback(*element))
                    return;
            }
            return;
        }

        // No element without the id holds a match.
        if (elements.is_empty())
            return;
        // NB: The subtrees of several elements with the id can nest, which only a walk of the whole tree visits once.
        if (elements.size() > 1 || !elements[0]) {
            for_each_in_subtree(root, callback);
            return;
        }
        // Every match is under the element with the id, or under its parent past a sibling combinator.
        Node* search_root = elements[0].ptr().ptr();
        if (lookup.kind == AncestorsParent)
            search_root = search_root->parent();
        if (!search_root || !(search_root == &root || is_under_root(*search_root))) {
            for_each_in_subtree(root, callback);
            return;
        }
        for_each_in_subtree(as<ParentNode>(*search_root), callback);
    }

    template<typename Callback>
    void for_each_in_subtree(ParentNode& root, Callback callback) const
    {
        CSS::SelectorFFI::rust_dom_selector_query_subtree(&m_query, node_to_ffi(&root), &callback, [](void* context, void const* element) {
            return (*static_cast<Callback*>(context))(const_cast<Element&>(element_from_ffi(element)));
        });
    }

    CSS::SelectorFFI::FfiDomSelectorQuery m_query {};
};

}

static ALWAYS_INLINE Element const* parent_element_of(Node const& node)
{
    auto const* parent = node.parent();
    return parent ? as_if<Element>(*parent) : nullptr;
}

bool SelectorQuery::matches_simple_compound(Element const& element, FastCompound const& compound, bool is_html_document)
{
    if (compound.tag_name && element.local_name().raw_identity() != compound.tag_name)
        return false;
    if (compound.id && (!element.id().has_value() || element.id()->raw_identity() != compound.id))
        return false;
    if (compound.class_count > 0) {
        auto classes = element.class_names();
        if (classes.size() < compound.class_count)
            return false;
        bool found_first = false;
        for (auto const& class_name : classes) {
            if (class_name.raw_identity() == compound.classes[0]) {
                found_first = true;
                break;
            }
        }
        if (!found_first)
            return false;
        if (compound.class_count == 2) {
            bool found_second = false;
            for (auto const& class_name : classes) {
                if (class_name.raw_identity() == compound.classes[1]) {
                    found_second = true;
                    break;
                }
            }
            if (!found_second)
                return false;
        }
    }
    if (compound.attribute_match_kind != AttributeMatchKind::None) {
        bool matched_attribute = false;
        for (auto const& attribute : element.attribute_list()) {
            if (attribute.name.local_name().raw_identity() != compound.attribute_name
                || (attribute.name.namespace_().has_value() && !attribute.name.namespace_()->is_empty()))
                continue;
            switch (compound.attribute_match_kind) {
            case AttributeMatchKind::HasAttribute:
                matched_attribute = true;
                break;
            case AttributeMatchKind::ExactCaseSensitive:
                matched_attribute = compound.attribute_value == 0
                    ? attribute.value.is_empty()
                    : attribute.value == Utf16FlyString::from_raw(compound.attribute_value);
                break;
            case AttributeMatchKind::ExactCaseInsensitive:
                matched_attribute = compound.attribute_value == 0
                    ? attribute.value.is_empty()
                    : attribute.value.equals_ignoring_ascii_case(Utf16FlyString::from_raw(compound.attribute_value));
                break;
            case AttributeMatchKind::ExactLegacyHtmlCaseInsensitive:
                if (compound.attribute_value == 0) {
                    matched_attribute = attribute.value.is_empty();
                } else {
                    auto expected = Utf16FlyString::from_raw(compound.attribute_value);
                    matched_attribute = (is_html_document && element.namespace_uri() == Namespace::HTML)
                        ? attribute.value.equals_ignoring_ascii_case(expected)
                        : attribute.value == expected;
                }
                break;
            case AttributeMatchKind::None:
                VERIFY_NOT_REACHED();
            }
            if (matched_attribute)
                break;
        }
        if (!matched_attribute)
            return false;
    }
    return true;
}

bool SelectorQuery::matches_fast(Element const& element) const
{
    bool const is_html_document = element.document().is_html_document();
    if (!matches_simple_compound(element, m_fast_subject, is_html_document))
        return false;
    if (m_fast_path_kind == FastPathKind::SimpleCompound)
        return true;
    auto const* parent = parent_element_of(element);
    if (m_fast_path_kind == FastPathKind::SimpleChildSimpleCompound)
        return parent && matches_simple_compound(*parent, m_fast_ancestor, is_html_document);
    for (; parent; parent = parent_element_of(*parent)) {
        if (matches_simple_compound(*parent, m_fast_ancestor, is_html_document))
            return true;
    }
    return false;
}

template<typename Callback>
void SelectorQuery::walk_subject_subtree(ParentNode& search_root, bool is_html_document, Callback&& callback) const
{
    auto const filter_bits = m_subject_filter_bits;
    auto const attr_filter_bits = m_subject_attribute_filter_bits;
    auto* next = search_root.next_in_pre_order(&search_root);
    while (next) {
        if (auto* element = as_if<Element>(*next)) {
            if ((filter_bits && (element->subtree_name_filter() & filter_bits) != filter_bits)
                || (attr_filter_bits && (element->subtree_attribute_filter() & attr_filter_bits) != attr_filter_bits)) {
                while (next != &search_root && !next->next_sibling())
                    next = next->parent();
                next = next == &search_root ? nullptr : next->next_sibling();
                continue;
            }
            if (matches_simple_compound(*element, m_fast_subject, is_html_document) && callback(*element))
                return;
        }
        next = next->next_in_pre_order(&search_root);
    }
}

template<typename Callback>
void SelectorQuery::for_each_fast_match(ParentNode& root, Callback&& callback) const
{
    auto const& document = root.document();
    bool const is_html_document = document.is_html_document();
    auto const& tree_root = root.root();
    u64 const all_filter_bits = m_subject_filter_bits | m_ancestor_filter_bits;
    u32 const all_attr_filter_bits = m_subject_attribute_filter_bits | m_ancestor_attribute_filter_bits;
    if (all_filter_bits != 0 || all_attr_filter_bits != 0) {
        u64 tree_filter = NumericLimits<u64>::max();
        u32 tree_attr_filter = NumericLimits<u32>::max();
        if (auto const* doc = as_if<Document>(tree_root)) {
            auto const* doc_element = doc->document_element();
            tree_filter = doc_element ? doc_element->subtree_name_filter() : 0;
            tree_attr_filter = doc_element ? doc_element->subtree_attribute_filter() : 0;
        } else if (auto const* root_element = as_if<Element>(tree_root)) {
            tree_filter = root_element->subtree_name_filter();
            tree_attr_filter = root_element->subtree_attribute_filter();
        }
        if ((tree_filter & all_filter_bits) != all_filter_bits
            || (tree_attr_filter & all_attr_filter_bits) != all_attr_filter_bits)
            return;
        if (auto const* root_element = as_if<Element>(root)) {
            if ((m_subject_filter_bits && (root_element->subtree_name_filter() & m_subject_filter_bits) != m_subject_filter_bits)
                || (m_subject_attribute_filter_bits && (root_element->subtree_attribute_filter() & m_subject_attribute_filter_bits) != m_subject_attribute_filter_bits))
                return;
        }
    }

    auto is_under_root = [&](Node const& node) {
        return &tree_root == &root ? &node != &root : node.is_descendant_of(root);
    };

    if (m_fast_subject.id && root.is_connected()) {
        auto const* shadow_root = as_if<ShadowRoot>(tree_root);
        auto const& element_by_id = shadow_root ? shadow_root->element_by_id() : document.element_by_id();
        auto elements = element_by_id.elements_with_id(Utf16FlyString::from_raw(m_fast_subject.id));
        if (elements.is_empty())
            return;
        if (elements.size() == 1) {
            if (auto* element = elements[0].ptr().ptr(); element && is_under_root(*element) && matches_fast(*element))
                callback(*element);
            return;
        }
        GC::RootVector<GC::Ref<Element>, 4> candidates;
        for (auto const& element : elements) {
            if (element && is_under_root(*element) && matches_fast(*element))
                candidates.append(*element);
        }
        if (candidates.size() > 1)
            quick_sort(candidates, [](auto const& a, auto const& b) { return a->is_before(*b); });
        for (auto& element : candidates) {
            if (callback(*element))
                return;
        }
        return;
    }

    if (m_fast_path_kind == FastPathKind::SimpleCompound) {
        walk_subject_subtree(root, is_html_document, callback);
        return;
    }

    if (m_fast_ancestor.id && root.is_connected()) {
        auto const* shadow_root = as_if<ShadowRoot>(tree_root);
        auto const& element_by_id = shadow_root ? shadow_root->element_by_id() : document.element_by_id();
        auto elements = element_by_id.elements_with_id(Utf16FlyString::from_raw(m_fast_ancestor.id));
        if (elements.is_empty())
            return;
        if (elements.size() == 1 && elements[0]) {
            auto* ancestor = elements[0].ptr().ptr();
            if (!matches_simple_compound(*ancestor, m_fast_ancestor, is_html_document))
                return;
            if (m_fast_path_kind == FastPathKind::SimpleChildSimpleCompound) {
                if (ancestor == &root || is_under_root(*ancestor)) {
                    for (auto* child = ancestor->first_child_of_type<Element>(); child; child = child->next_element_sibling()) {
                        if (matches_simple_compound(*child, m_fast_subject, is_html_document) && callback(*child))
                            return;
                    }
                }
                return;
            }
            if (ancestor == &root || is_under_root(*ancestor)) {
                walk_subject_subtree(*ancestor, is_html_document, callback);
                return;
            }
            if (root.is_descendant_of(*ancestor)) {
                walk_subject_subtree(root, is_html_document, callback);
                return;
            }
            return;
        }
    }

    if (m_fast_path_kind == FastPathKind::SimpleChildSimpleCompound) {
        walk_subject_subtree(root, is_html_document, [&](Element& element) {
            auto const* parent = parent_element_of(element);
            return parent && matches_simple_compound(*parent, m_fast_ancestor, is_html_document) && callback(element);
        });
        return;
    }

    // SimpleDescendantSimpleCompound: check once whether root or an ancestor of root satisfies the ancestor compound.
    for (auto const* ancestor = as_if<Element>(root); ancestor; ancestor = parent_element_of(*ancestor)) {
        if (matches_simple_compound(*ancestor, m_fast_ancestor, is_html_document)) {
            walk_subject_subtree(root, is_html_document, callback);
            return;
        }
    }
    if (auto const* root_element = as_if<Element>(root)) {
        if ((m_ancestor_filter_bits && (root_element->subtree_name_filter() & m_ancestor_filter_bits) != m_ancestor_filter_bits)
            || (m_ancestor_attribute_filter_bits && (root_element->subtree_attribute_filter() & m_ancestor_attribute_filter_bits) != m_ancestor_attribute_filter_bits))
            return;
    }
    walk_subject_subtree(root, is_html_document, [&](Element& element) {
        for (auto const* parent = parent_element_of(element); parent && parent != &root; parent = parent_element_of(*parent)) {
            if (matches_simple_compound(*parent, m_fast_ancestor, is_html_document))
                return callback(element);
        }
        return false;
    });
}

bool SelectorQuery::matches(Element const& element, ParentNode const& scope) const
{
    if (m_matches_every_element)
        return true;
    if (m_fast_path_kind != FastPathKind::None && !element.document().in_quirks_mode())
        return matches_fast(element);
    return DomSelectorQuery { program(element.document()), element, scope }.matches(element);
}

GC::Ptr<Element const> SelectorQuery::closest(Element const& element) const
{
    if (m_matches_every_element)
        return &element;
    if (m_fast_path_kind != FastPathKind::None && !element.document().in_quirks_mode()) {
        for (auto const* current = &element; current; current = parent_element_of(*current)) {
            if (matches_fast(*current))
                return current;
        }
        return nullptr;
    }
    return DomSelectorQuery { program(element.document()), element, element }.closest(element);
}

// https://dom.spec.whatwg.org/#scope-match-a-selectors-string
// This implements step 3, "match a selector against a tree" with the parsed selectors,
// stopping at the first match.
GC::Ptr<Element> SelectorQuery::query_first(ParentNode& root) const
{
    if (!root.first_element_child())
        return nullptr;

    auto& document = root.document();
    if (m_is_result_cacheable) {
        if (auto const* cached_elements = document.query_selector_result_cache().get(root, *this, QuerySelectorResultCache::ResultType::FirstOnly))
            return cached_elements->is_empty() ? nullptr : &cached_elements->first();
    }

    // FIXME: This should be shadow-including. https://drafts.csswg.org/selectors-4/#match-a-selector-against-a-tree
    GC::Ptr<Element> result;
    if (m_matches_every_element) {
        result = root.first_child_of_type<Element>();
    } else if (m_fast_path_kind != FastPathKind::None && !document.in_quirks_mode()) {
        for_each_fast_match(root, [&](Element& element) {
            result = element;
            return true;
        });
    } else {
        result = DomSelectorQuery { program(document), root, root }.first(root);
    }

    if (m_is_result_cacheable) {
        auto& cache = document.query_selector_result_cache();
        if (result) {
            GC::Ref<Node> node = *result;
            cache.set(root, *this, QuerySelectorResultCache::ResultType::FirstOnly, { &node, 1 });
        } else {
            cache.set(root, *this, QuerySelectorResultCache::ResultType::FirstOnly, {});
        }
    }
    return result;
}

static GC::Ref<NodeList> create_node_list(GC::WeakVector<Element> const& elements)
{
    GC::RootVector<GC::Ref<Node>> nodes;
    nodes.ensure_capacity(elements.size());
    for (auto& element : elements)
        nodes.unchecked_append(element);
    return StaticNodeList::create(move(nodes));
}

// https://dom.spec.whatwg.org/#scope-match-a-selectors-string
// This implements step 3, "match a selector against a tree" with the parsed selectors.
GC::Ref<NodeList> SelectorQuery::query_all(ParentNode& root) const
{
    if (!root.first_element_child())
        return StaticNodeList::create(ReadonlySpan<GC::Ref<Node>> {});

    auto& document = root.document();
    if (m_is_result_cacheable) {
        if (auto const* cached_elements = document.query_selector_result_cache().get(root, *this, QuerySelectorResultCache::ResultType::All))
            return create_node_list(*cached_elements);
    }

    // FIXME: This should be shadow-including. https://drafts.csswg.org/selectors-4/#match-a-selector-against-a-tree
    GC::RootVector<GC::Ref<Node>> nodes;
    if (m_matches_every_element) {
        root.for_each_in_subtree_of_type<Element>([&](auto& element) {
            nodes.append(element);
            return TraversalDecision::Continue;
        });
    } else if (m_fast_path_kind != FastPathKind::None && !document.in_quirks_mode()) {
        for_each_fast_match(root, [&](Element& element) {
            nodes.append(element);
            return false;
        });
    } else {
        DomSelectorQuery { program(document), root, root }.collect(root, nodes);
    }

    if (m_is_result_cacheable)
        document.query_selector_result_cache().set(root, *this, QuerySelectorResultCache::ResultType::All, nodes);
    return StaticNodeList::create(move(nodes));
}

GC_DEFINE_ALLOCATOR(QuerySelectorResultCache);

QuerySelectorResultCache::QuerySelectorResultCache()
    : GC::WeakContainer(heap())
{
}

GC::WeakVector<Element> const* QuerySelectorResultCache::get(ParentNode const& root, SelectorQuery const& query, ResultType result_type)
{
    auto it = m_entries.find(Key { reinterpret_cast<FlatPtr>(&root), &query });
    if (it == m_entries.end())
        return nullptr;

    auto const& entry = it->value;
    if (entry.root.ptr().ptr() != &root
        || entry.dom_tree_version != root.dom_tree_version()
        || (query.depends_on_character_data() && entry.character_data_version != root.character_data_version())) {
        m_entries.remove(it);
        return nullptr;
    }
    if (result_type == ResultType::All && entry.result_type != ResultType::All)
        return nullptr;

    return &entry.elements;
}

void QuerySelectorResultCache::set(ParentNode const& root, SelectorQuery const& query, ResultType result_type, ReadonlySpan<GC::Ref<Node>> nodes)
{
    static constexpr size_t MAX_QUERY_SELECTOR_RESULT_CACHE_SIZE = 64;

    auto key = Key { reinterpret_cast<FlatPtr>(&root), &query };
    if (!m_entries.contains(key) && m_entries.size() >= MAX_QUERY_SELECTOR_RESULT_CACHE_SIZE)
        m_entries.remove(m_entries.begin());

    Entry entry { root, query, root.dom_tree_version(), root.character_data_version(), result_type, {} };
    entry.elements.ensure_capacity(nodes.size());
    for (auto node : nodes)
        entry.elements.append(static_cast<Element&>(*node));
    m_entries.set(key, move(entry));
}

void QuerySelectorResultCache::remove_dead_cells(Badge<GC::Heap>)
{
    m_entries.remove_all_matching([&](Key const&, Entry& entry) {
        return entry.elements.remove_dead_cells(heap());
    });
}

}
