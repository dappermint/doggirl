/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

#pragma once

#include <AK/Noncopyable.h>
#include <AK/Optional.h>
#include <AK/Vector.h>
#include <LibGC/Ptr.h>
#include <LibGC/RootVector.h>
#include <LibWeb/Forward.h>

namespace Web::DOM {

class SubtreeInsertionScope {
    AK_MAKE_NONCOPYABLE(SubtreeInsertionScope);
    AK_MAKE_NONMOVABLE(SubtreeInsertionScope);

public:
    explicit SubtreeInsertionScope(Node& parent);
    ~SubtreeInsertionScope();

    Node& parent() const { return m_parent; }
    GC::Ptr<HTML::HTMLFormElement> nearest_inclusive_form_ancestor_of_parent();
    ReadonlySpan<GC::Ref<Element>> inclusive_fieldset_ancestors_of_parent();
    bool keyboard_scroll_tracks_document() const { return m_keyboard_scroll_tracks_document; }
    bool document_may_have_blocking_wheel_event_listener() const { return m_document_may_have_blocking_wheel_event_listener; }
    bool document_has_auto_directionality() const { return m_document_has_auto_directionality; }
    void note_auto_directionality() { m_document_has_auto_directionality = true; }
    bool document_has_inert_element() const { return m_document_has_inert_element; }
    bool content_blocker_has_cosmetic_rules() const { return m_content_blocker_has_cosmetic_rules; }
    bool descendants_covered_by_subtree_arrival() const { return m_descendants_covered_by_subtree_arrival; }
    void set_descendants_covered_by_subtree_arrival(bool value) { m_descendants_covered_by_subtree_arrival = value; }
    bool may_be_in_shadow_tree() const { return m_parent_is_in_shadow_tree || m_saw_shadow_root; }
    void note_shadow_root_visited() { m_saw_shadow_root = true; }
    void reset_for_next_child()
    {
        m_descendants_covered_by_subtree_arrival = false;
        m_saw_shadow_root = false;
    }

private:
    GC::Ref<Node> m_parent;
    GC::Ref<Document> m_document;
    SubtreeInsertionScope* m_enclosing_scope { nullptr };
    Optional<GC::Ptr<HTML::HTMLFormElement>> m_nearest_inclusive_form_ancestor_of_parent;
    Optional<GC::RootVector<GC::Ref<Element>>> m_inclusive_fieldset_ancestors_of_parent;
    bool m_has_collected_inclusive_fieldset_ancestors_of_parent { false };
    bool m_keyboard_scroll_tracks_document { false };
    bool m_document_may_have_blocking_wheel_event_listener { false };
    bool m_document_has_auto_directionality { false };
    bool m_document_has_inert_element { false };
    bool m_content_blocker_has_cosmetic_rules { false };
    bool m_descendants_covered_by_subtree_arrival { false };
    bool m_parent_is_in_shadow_tree { false };
    bool m_saw_shadow_root { false };
};

}
