/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! The scanner of the fragment fast path.
//!
//! Markup given to innerHTML is very often a small subset of what goes in a body: a few kinds of elements, properly
//! nested, with quoted attribute values and no character references. The scanner reads such markup once and writes
//! down what the tree builder would have made of it as a flat list of operations, which the host then builds nodes
//! from without a tokenizer or a tree builder. Anything else is turned down before the host has made a single node,
//! and goes to the general parser.

use crate::code_units::RustFfiHtmlUtf16View;
use crate::known_names::known_attribute_name;
use crate::known_names::tag_id;
#[cfg(test)]
use crate::known_names::tag_id_for_name;
use crate::scan;
use crate::scan::CodeUnit;
use crate::token::TagId;
#[cfg(test)]
use crate::token::TagSet;
#[cfg(test)]
use crate::token::tag_set;
use std::cell::Cell;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RustFfiHtmlFragmentOperationKind {
    /// Make an element and the attributes at `start..start + length` of the attribute list, and append it to the
    /// innermost open element. It is then the innermost open element.
    OpenElement,
    /// The same for an element that takes no children, which is not left open.
    AppendVoidElement,
    /// Close the innermost open element.
    CloseElement,
    /// Append a text node with the code units at `start..start + length` of the input.
    AppendText,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RustFfiHtmlFragmentTag {
    None = 0,
    A,
    Article,
    Aside,
    B,
    Big,
    Br,
    Button,
    Code,
    Div,
    Em,
    Footer,
    Header,
    I,
    Input,
    Label,
    Li,
    Main,
    Nav,
    Ol,
    S,
    Section,
    Small,
    Span,
    Strike,
    Strong,
    Tt,
    U,
    Ul,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RustFfiHtmlFragmentOperation {
    pub kind: RustFfiHtmlFragmentOperationKind,
    /// Whether the text of `AppendText` is all ASCII.
    pub is_ascii: bool,
    /// Compact supported tag kind for `OpenElement` and `AppendVoidElement`, or `None` for other operations.
    pub tag_kind: RustFfiHtmlFragmentTag,
    pub start: u32,
    pub length: u32,
}

/// Compact classification of hot attribute names scanned by the fragment fast path.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RustFfiHtmlFragmentAttributeKind {
    None = 0,
    Class,
    Id,
    Type,
    Value,
    Checked,
    Href,
    Style,
    Name,
    For,
    Title,
    Disabled,
    Placeholder,
    Target,
    Rel,
    Hidden,
    Tabindex,
    Autocomplete,
    DataId,
    OtherData,
}

/// Compact classification of supported `<input type="...">` values scanned by the fragment fast path.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RustFfiHtmlFragmentInputType {
    None = 0,
    Text,
    Checkbox,
}

/// An attribute of an element to make. The name and the value are ranges of code units of the input.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RustFfiHtmlFragmentAttribute {
    /// The identity of the `Utf16FlyString` with the name if it is one the engine knows, and otherwise 0.
    pub known_name: usize,
    pub name_start: u32,
    pub name_length: u32,
    pub value_start: u32,
    pub value_length: u32,
    pub hot_kind: RustFfiHtmlFragmentAttributeKind,
    pub value_is_ascii: bool,
    pub input_type: RustFfiHtmlFragmentInputType,
}

/// What the scanner made of a fragment. `tape` is null if the fragment is not for the fast path, and otherwise must
/// be given to `rust_html_fragment_tape_release` once the operations have been read.
#[repr(C)]
pub struct RustFfiHtmlFragmentScan {
    pub tape: *mut RustHtmlFragmentTape,
    pub operations: *const RustFfiHtmlFragmentOperation,
    pub operation_count: usize,
    pub attributes: *const RustFfiHtmlFragmentAttribute,
    pub attribute_count: usize,
}

#[derive(Default)]
pub struct RustHtmlFragmentTape {
    operations: Vec<RustFfiHtmlFragmentOperation>,
    attributes: Vec<RustFfiHtmlFragmentAttribute>,
    open_elements: Vec<RustFfiHtmlFragmentTag>,
}

thread_local! {
    // NB: One tape is kept between fragments, so that scanning a fragment does not allocate.
    static SPARE_TAPE: Cell<Option<Box<RustHtmlFragmentTape>>> = const { Cell::new(None) };
}

// NB: A tape that an unusually large fragment grew past these sizes is freed, so that the spare one stays small.
const MAX_SPARE_TAPE_OPERATIONS: usize = 4096;
const MAX_SPARE_TAPE_ATTRIBUTES: usize = 4096;

fn keep_as_spare_tape(tape: Box<RustHtmlFragmentTape>) {
    if tape.operations.capacity() <= MAX_SPARE_TAPE_OPERATIONS
        && tape.attributes.capacity() <= MAX_SPARE_TAPE_ATTRIBUTES
    {
        SPARE_TAPE.set(Some(tape));
    }
}

/// Scans a fragment for the fast path.
///
/// # Safety
/// `input` must point at `input.length` readable code units.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_html_fragment_scan(input: RustFfiHtmlUtf16View) -> RustFfiHtmlFragmentScan {
    let mut tape = SPARE_TAPE.take().unwrap_or_default();
    // SAFETY: The caller promises that `input` points at `input.length` code units.
    let accepted = unsafe {
        if input.length == 0 {
            scan::<u8>(&[], &mut tape)
        } else if !input.latin1.is_null() {
            scan(ak::slice_from_raw_parts(input.latin1, input.length), &mut tape)
        } else {
            scan(ak::slice_from_raw_parts(input.utf16, input.length), &mut tape)
        }
    };
    if !accepted {
        keep_as_spare_tape(tape);
        return RustFfiHtmlFragmentScan {
            tape: std::ptr::null_mut(),
            operations: std::ptr::null(),
            operation_count: 0,
            attributes: std::ptr::null(),
            attribute_count: 0,
        };
    }
    RustFfiHtmlFragmentScan {
        operations: tape.operations.as_ptr(),
        operation_count: tape.operations.len(),
        attributes: tape.attributes.as_ptr(),
        attribute_count: tape.attributes.len(),
        tape: Box::into_raw(tape),
    }
}

/// Gives back the tape of a scan whose operations have been read.
///
/// # Safety
/// `tape` must be the tape of a `RustFfiHtmlFragmentScan` that has not been released yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_html_fragment_tape_release(tape: *mut RustHtmlFragmentTape) {
    // SAFETY: The caller promises that `tape` came from `rust_html_fragment_scan` and is released once.
    keep_as_spare_tape(unsafe { Box::from_raw(tape) });
}

/// Lowercase ASCII tag name bytes indexed by `RustFfiHtmlFragmentTag as usize`.
const TAG_BYTES: [&[u8]; 29] = [
    b"", b"a", b"article", b"aside", b"b", b"big", b"br", b"button", b"code", b"div", b"em", b"footer", b"header",
    b"i", b"input", b"label", b"li", b"main", b"nav", b"ol", b"s", b"section", b"small", b"span", b"strike", b"strong",
    b"tt", b"u", b"ul",
];

#[cfg(test)]
macro_rules! supported_tags {
    ($($name:tt),+ $(,)?) => {
        const SUPPORTED_TAGS: &TagSet = tag_set![$($name),+];
        const SUPPORTED_TAG_NAMES: &[&str] = &[$($name),+];
    };
}

#[cfg(test)]
supported_tags![
    "a", "article", "aside", "b", "big", "br", "button", "code", "div", "em", "footer", "header", "i", "input",
    "label", "li", "main", "nav", "ol", "s", "section", "small", "span", "strike", "strong", "tt", "u", "ul",
];

const MAX_OPEN_ELEMENTS: usize = 128;
const MAX_ATTRIBUTES: usize = 64;

/// Lookup table for valid lowercase ASCII tag/attribute name characters (`a..=z`, `0..=9`, `-`).
const NAME_CHAR_TABLE: [bool; 256] = {
    let mut table = [false; 256];
    let mut i = b'a' as usize;
    while i <= b'z' as usize {
        table[i] = true;
        i += 1;
    }
    let mut d = b'0' as usize;
    while d <= b'9' as usize {
        table[d] = true;
        d += 1;
    }
    table[b'-' as usize] = true;
    table
};

/// Lookup table for ASCII tag whitespace (`\t`, `\n`, `\x0C`, ` `).
const TAG_WHITESPACE_TABLE: [bool; 256] = {
    let mut table = [false; 256];
    table[0x09] = true;
    table[0x0A] = true;
    table[0x0C] = true;
    table[0x20] = true;
    table
};

/// The whitespace between the parts of a tag.
#[inline(always)]
fn is_tag_whitespace(code_unit: u16) -> bool {
    code_unit <= 0x20 && TAG_WHITESPACE_TABLE[code_unit as usize]
}

fn equals_ascii<T: CodeUnit>(code_units: &[T], ascii: &[u8]) -> bool {
    code_units.len() == ascii.len()
        && code_units
            .iter()
            .zip(ascii)
            .all(|(&code_unit, &byte)| code_unit.into() == u16::from(byte))
}

fn scan<T: CodeUnit>(input: &[T], tape: &mut RustHtmlFragmentTape) -> bool {
    tape.operations.clear();
    tape.attributes.clear();
    tape.open_elements.clear();
    // NB: The tape has offsets into the input as 32-bit numbers.
    if u32::try_from(input.len()).is_err() {
        return false;
    }
    Scanner {
        input,
        offset: 0,
        #[cfg(not(test))]
        hot_attributes: hot_attribute_identities(),
        tape,
    }
    .scan()
}

struct Scanner<'a, T> {
    input: &'a [T],
    offset: usize,
    #[cfg(not(test))]
    hot_attributes: &'static [usize; HOT_ATTRIBUTE_NAMES.len()],
    tape: &'a mut RustHtmlFragmentTape,
}

impl<'a, T: CodeUnit> Scanner<'a, T> {
    fn scan(&mut self) -> bool {
        while self.offset < self.input.len() {
            let scanned = if self.peek() != u16::from(b'<') {
                self.scan_text()
            } else {
                self.offset += 1;
                if self.consume(b'/') {
                    self.scan_end_tag()
                } else {
                    self.scan_start_tag()
                }
            };
            if !scanned {
                return false;
            }
        }
        self.tape.open_elements.is_empty()
    }

    fn scan_text(&mut self) -> bool {
        // NB: Character references and input preprocessing use the general parser, which also replaces a surrogate
        //     that isn't half of a pair with U+FFFD. The run ends at all of those, so it has to end at a tag or at the
        //     end of the input.
        let run = scan::data_run(&self.input[self.offset..]);
        let end = self.offset + run.length;
        if end < self.input.len() && self.input[end].into() != u16::from(b'<') {
            return false;
        }
        self.tape.operations.push(RustFfiHtmlFragmentOperation {
            kind: RustFfiHtmlFragmentOperationKind::AppendText,
            is_ascii: run.is_ascii,
            tag_kind: RustFfiHtmlFragmentTag::None,
            start: self.offset as u32,
            length: run.length as u32,
        });
        self.offset = end;
        true
    }

    #[inline]
    fn scan_end_tag(&mut self) -> bool {
        let Some(&expected_tag) = self.tape.open_elements.last() else {
            return false;
        };
        let expected = TAG_BYTES[expected_tag as usize];
        let end = self.offset + expected.len();
        if end > self.input.len() || !equals_ascii(&self.input[self.offset..end], expected) {
            return false;
        }
        self.offset = end;
        if !self.consume(b'>') {
            let next = self.peek();
            if next <= 0x7A && NAME_CHAR_TABLE[next as usize] {
                return false;
            }
            self.skip_whitespace();
            if !self.consume(b'>') {
                return false;
            }
        }
        self.tape.open_elements.pop();
        self.tape.operations.push(RustFfiHtmlFragmentOperation {
            kind: RustFfiHtmlFragmentOperationKind::CloseElement,
            is_ascii: false,
            tag_kind: RustFfiHtmlFragmentTag::None,
            start: 0,
            length: 0,
        });
        true
    }

    fn scan_start_tag(&mut self) -> bool {
        let Some(tag_kind) = self.scan_supported_tag() else {
            return false;
        };
        let open_elements = &self.tape.open_elements;

        // Leave implicit closing of anchors, buttons, and list items to the general parser.
        if (tag_kind == RustFfiHtmlFragmentTag::A || tag_kind == RustFfiHtmlFragmentTag::Button)
            && open_elements.contains(&tag_kind)
        {
            return false;
        }
        if tag_kind == RustFfiHtmlFragmentTag::Li {
            for &ancestor in open_elements.iter().rev() {
                if ancestor == RustFfiHtmlFragmentTag::Li {
                    return false;
                }
                // A nested list stops the in-body rule from closing an outer list item.
                if ancestor == RustFfiHtmlFragmentTag::Ul || ancestor == RustFfiHtmlFragmentTag::Ol {
                    break;
                }
            }
        }
        if open_elements.len() >= MAX_OPEN_ELEMENTS {
            return false;
        }

        let first_attribute = self.tape.attributes.len();
        let mut seen_hot_attrs: u32 = 0;
        loop {
            self.skip_whitespace();
            let next = self.peek();
            if next == u16::from(b'>') || next == u16::from(b'/') {
                break;
            }
            if !self.scan_attribute(tag_kind, first_attribute, &mut seen_hot_attrs) {
                return false;
            }
        }

        let is_void = tag_kind == RustFfiHtmlFragmentTag::Br || tag_kind == RustFfiHtmlFragmentTag::Input;
        if self.consume(b'/') && !is_void {
            return false;
        }
        if !self.consume(b'>') {
            return false;
        }

        self.tape.operations.push(RustFfiHtmlFragmentOperation {
            kind: if is_void {
                RustFfiHtmlFragmentOperationKind::AppendVoidElement
            } else {
                RustFfiHtmlFragmentOperationKind::OpenElement
            },
            is_ascii: false,
            tag_kind,
            start: first_attribute as u32,
            length: (self.tape.attributes.len() - first_attribute) as u32,
        });
        if !is_void {
            self.tape.open_elements.push(tag_kind);
        }
        true
    }

    /// Scans one attribute of a start tag of `tag_kind`, whose attributes so far start at `first_attribute`.
    fn scan_attribute(
        &mut self,
        tag_kind: RustFfiHtmlFragmentTag,
        first_attribute: usize,
        seen_hot_attrs: &mut u32,
    ) -> bool {
        let input = self.input;
        let name_start = self.offset;
        let mut name_buf = [0u8; 32];
        let (name_len, name_bytes) = self.scan_name_into(&mut name_buf);
        // NB: Customized built-in elements are made by the general parser.
        if name_len == 0
            || name_bytes == Some(b"is".as_slice())
            || self.tape.attributes.len() - first_attribute >= MAX_ATTRIBUTES
        {
            return false;
        }

        let (hot_kind, known_name) = match name_bytes {
            Some(bytes) => self.classify_attribute_name_bytes(bytes),
            None => (RustFfiHtmlFragmentAttributeKind::None, 0),
        };

        // NB: The tokenizer drops an attribute whose name the tag already has, which the general parser sees to.
        if hot_kind != RustFfiHtmlFragmentAttributeKind::None && hot_kind != RustFfiHtmlFragmentAttributeKind::OtherData
        {
            let bit = 1_u32 << (hot_kind as u8);
            if *seen_hot_attrs & bit != 0 {
                return false;
            }
            *seen_hot_attrs |= bit;
        } else if self.tape.attributes.len() > first_attribute {
            let name = &input[name_start..name_start + name_len];
            let is_duplicate = self.tape.attributes[first_attribute..].iter().any(|attribute| {
                (attribute.hot_kind == RustFfiHtmlFragmentAttributeKind::None
                    || attribute.hot_kind == RustFfiHtmlFragmentAttributeKind::OtherData)
                    && attribute.name_length as usize == name_len
                    && input[attribute.name_start as usize..attribute.name_start as usize + name_len] == *name
            });
            if is_duplicate {
                return false;
            }
        }

        self.skip_whitespace();
        let mut value = scan::Run {
            length: 0,
            is_ascii: true,
        };
        let mut value_start = self.offset;
        if self.consume(b'=') {
            self.skip_whitespace();
            let quote = self.peek();
            if quote != u16::from(b'\'') && quote != u16::from(b'"') {
                return false;
            }
            self.offset += 1;
            value_start = self.offset;
            // NB: As with text, the run ends at everything that the general parser has to see to, so it has to end at
            //     the closing quote.
            value = scan::quoted_attribute_value_run(&input[value_start..], quote as u8);
            self.offset = value_start + value.length;
            if !self.consume(quote as u8) {
                return false;
            }
        }

        // NB: Other input types have insertion steps or children the fast path doesn't cover.
        let mut input_type = RustFfiHtmlFragmentInputType::None;
        if tag_kind == RustFfiHtmlFragmentTag::Input && hot_kind == RustFfiHtmlFragmentAttributeKind::Type {
            let value_code_units = &input[value_start..value_start + value.length];
            if equals_ascii(value_code_units, b"text") {
                input_type = RustFfiHtmlFragmentInputType::Text;
            } else if equals_ascii(value_code_units, b"checkbox") {
                input_type = RustFfiHtmlFragmentInputType::Checkbox;
            } else {
                return false;
            }
        }

        self.tape.attributes.push(RustFfiHtmlFragmentAttribute {
            known_name,
            name_start: name_start as u32,
            name_length: name_len as u32,
            value_start: value_start as u32,
            value_length: value.length as u32,
            hot_kind,
            value_is_ascii: value.is_ascii,
            input_type,
        });
        true
    }

    #[inline]
    fn scan_supported_tag(&mut self) -> Option<RustFfiHtmlFragmentTag> {
        let mut buffer = [0u8; 16];
        let (_, bytes) = self.scan_name_into(&mut buffer);
        supported_tag_for_name(bytes?).map(|(_, tag_kind)| tag_kind)
    }

    /// Scans a tag or attribute name of lowercase ASCII letters, digits and hyphens, returning its
    /// length and, if it fits in `buffer`, its ASCII bytes in a single pass.
    #[inline]
    fn scan_name_into<'b>(&mut self, buffer: &'b mut [u8]) -> (usize, Option<&'b [u8]>)
    where
        'a: 'b,
    {
        let input = self.input;
        let start = self.offset;
        let mut advance = 0;
        if let Some(bytes) = T::as_byte_slice(&input[start..]) {
            while advance < bytes.len() && NAME_CHAR_TABLE[bytes[advance] as usize] {
                advance += 1;
            }
            self.offset = start + advance;
            let slice = (advance <= buffer.len()).then_some(&bytes[..advance]);
            return (advance, slice);
        }
        let buf_len = buffer.len();
        for &code_unit in &input[start..] {
            let unit = code_unit.into();
            if unit > 0x7A || !NAME_CHAR_TABLE[unit as usize] {
                break;
            }
            if advance < buf_len {
                buffer[advance] = unit as u8;
            }
            advance += 1;
        }
        self.offset = start + advance;
        let slice = (advance <= buf_len).then_some(&buffer[..advance]);
        (advance, slice)
    }

    /// The code unit at the offset, or U+0000 NULL at the end of the input. Nothing the scanner takes has a NULL in it.
    #[inline(always)]
    fn peek(&self) -> u16 {
        self.input.get(self.offset).map_or(0, |&code_unit| code_unit.into())
    }

    #[inline(always)]
    fn consume(&mut self, byte: u8) -> bool {
        if self.offset == self.input.len() || self.peek() != u16::from(byte) {
            return false;
        }
        self.offset += 1;
        true
    }

    #[inline(always)]
    fn skip_whitespace(&mut self) {
        if self.offset >= self.input.len() || self.input[self.offset].into() > 0x20 {
            return;
        }
        let mut advance = 0;
        if let Some(bytes) = T::as_byte_slice(&self.input[self.offset..]) {
            while advance < bytes.len() && TAG_WHITESPACE_TABLE[bytes[advance] as usize] {
                advance += 1;
            }
        } else {
            for &code_unit in &self.input[self.offset..] {
                if !is_tag_whitespace(code_unit.into()) {
                    break;
                }
                advance += 1;
            }
        }
        self.offset += advance;
    }

    fn classify_attribute_name_bytes(&self, bytes: &[u8]) -> (RustFfiHtmlFragmentAttributeKind, usize) {
        // No HTML known attribute starts with "data-".
        if bytes.len() >= 5 && &bytes[..5] == b"data-" {
            let kind = if bytes == b"data-id" {
                RustFfiHtmlFragmentAttributeKind::DataId
            } else {
                RustFfiHtmlFragmentAttributeKind::OtherData
            };
            return (kind, 0);
        }
        if let Some(index) = hot_attribute_index(bytes) {
            let kind = HOT_ATTRIBUTE_KINDS[index];
            #[cfg(not(test))]
            let identity = self.hot_attributes[index];
            #[cfg(test)]
            let identity = 1;
            return (kind, identity);
        }
        #[cfg(not(test))]
        {
            (
                RustFfiHtmlFragmentAttributeKind::None,
                known_attribute_name(bytes).map_or(0, |name| name.raw_identity()),
            )
        }
        // NB: There are no fly strings in the standalone tests, which only tell known names from other names.
        #[cfg(test)]
        {
            (
                RustFfiHtmlFragmentAttributeKind::None,
                usize::from(known_attribute_name(bytes).is_some()),
            )
        }
    }
}

#[inline]
fn hot_attribute_index(bytes: &[u8]) -> Option<usize> {
    match bytes.len() {
        2 => (bytes == b"id").then_some(1),
        3 => match bytes {
            b"for" => Some(8),
            b"rel" => Some(13),
            _ => None,
        },
        4 => match bytes {
            b"type" => Some(2),
            b"href" => Some(5),
            b"name" => Some(7),
            _ => None,
        },
        5 => match bytes {
            b"class" => Some(0),
            b"value" => Some(3),
            b"style" => Some(6),
            b"title" => Some(9),
            _ => None,
        },
        6 => match bytes {
            b"target" => Some(12),
            b"hidden" => Some(14),
            _ => None,
        },
        7 => (bytes == b"checked").then_some(4),
        8 => match bytes {
            b"disabled" => Some(10),
            b"tabindex" => Some(15),
            _ => None,
        },
        11 => (bytes == b"placeholder").then_some(11),
        12 => (bytes == b"autocomplete").then_some(16),
        _ => None,
    }
}

#[inline]
fn supported_tag_for_name(name: &[u8]) -> Option<(TagId, RustFfiHtmlFragmentTag)> {
    use RustFfiHtmlFragmentTag as Kind;
    match name.len() {
        1 => match name[0] {
            b'a' => Some((tag_id!("a"), Kind::A)),
            b'b' => Some((tag_id!("b"), Kind::B)),
            b'i' => Some((tag_id!("i"), Kind::I)),
            b's' => Some((tag_id!("s"), Kind::S)),
            b'u' => Some((tag_id!("u"), Kind::U)),
            _ => None,
        },
        2 => match name {
            b"li" => Some((tag_id!("li"), Kind::Li)),
            b"ul" => Some((tag_id!("ul"), Kind::Ul)),
            b"br" => Some((tag_id!("br"), Kind::Br)),
            b"em" => Some((tag_id!("em"), Kind::Em)),
            b"ol" => Some((tag_id!("ol"), Kind::Ol)),
            b"tt" => Some((tag_id!("tt"), Kind::Tt)),
            _ => None,
        },
        3 => match name {
            b"div" => Some((tag_id!("div"), Kind::Div)),
            b"nav" => Some((tag_id!("nav"), Kind::Nav)),
            b"big" => Some((tag_id!("big"), Kind::Big)),
            _ => None,
        },
        4 => match name {
            b"span" => Some((tag_id!("span"), Kind::Span)),
            b"main" => Some((tag_id!("main"), Kind::Main)),
            b"code" => Some((tag_id!("code"), Kind::Code)),
            _ => None,
        },
        5 => match name {
            b"input" => Some((tag_id!("input"), Kind::Input)),
            b"label" => Some((tag_id!("label"), Kind::Label)),
            b"aside" => Some((tag_id!("aside"), Kind::Aside)),
            b"small" => Some((tag_id!("small"), Kind::Small)),
            _ => None,
        },
        6 => match name {
            b"button" => Some((tag_id!("button"), Kind::Button)),
            b"footer" => Some((tag_id!("footer"), Kind::Footer)),
            b"header" => Some((tag_id!("header"), Kind::Header)),
            b"strong" => Some((tag_id!("strong"), Kind::Strong)),
            b"strike" => Some((tag_id!("strike"), Kind::Strike)),
            _ => None,
        },
        7 => match name {
            b"section" => Some((tag_id!("section"), Kind::Section)),
            b"article" => Some((tag_id!("article"), Kind::Article)),
            _ => None,
        },
        _ => None,
    }
}

const HOT_ATTRIBUTE_NAMES: [&[u8]; 17] = [
    b"class",
    b"id",
    b"type",
    b"value",
    b"checked",
    b"href",
    b"style",
    b"name",
    b"for",
    b"title",
    b"disabled",
    b"placeholder",
    b"target",
    b"rel",
    b"hidden",
    b"tabindex",
    b"autocomplete",
];

const HOT_ATTRIBUTE_KINDS: [RustFfiHtmlFragmentAttributeKind; HOT_ATTRIBUTE_NAMES.len()] = [
    RustFfiHtmlFragmentAttributeKind::Class,
    RustFfiHtmlFragmentAttributeKind::Id,
    RustFfiHtmlFragmentAttributeKind::Type,
    RustFfiHtmlFragmentAttributeKind::Value,
    RustFfiHtmlFragmentAttributeKind::Checked,
    RustFfiHtmlFragmentAttributeKind::Href,
    RustFfiHtmlFragmentAttributeKind::Style,
    RustFfiHtmlFragmentAttributeKind::Name,
    RustFfiHtmlFragmentAttributeKind::For,
    RustFfiHtmlFragmentAttributeKind::Title,
    RustFfiHtmlFragmentAttributeKind::Disabled,
    RustFfiHtmlFragmentAttributeKind::Placeholder,
    RustFfiHtmlFragmentAttributeKind::Target,
    RustFfiHtmlFragmentAttributeKind::Rel,
    RustFfiHtmlFragmentAttributeKind::Hidden,
    RustFfiHtmlFragmentAttributeKind::Tabindex,
    RustFfiHtmlFragmentAttributeKind::Autocomplete,
];

#[cfg(not(test))]
fn hot_attribute_identities() -> &'static [usize; HOT_ATTRIBUTE_NAMES.len()] {
    static IDENTITIES: std::sync::OnceLock<[usize; HOT_ATTRIBUTE_NAMES.len()]> = std::sync::OnceLock::new();
    IDENTITIES.get_or_init(|| {
        std::array::from_fn(|index| {
            known_attribute_name(HOT_ATTRIBUTE_NAMES[index]).map_or(0, |name| name.raw_identity())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::RustFfiHtmlFragmentOperationKind::*;
    use super::*;

    /// What the scanner makes of `input`, as one line per operation, or `None` if it turns the input down.
    fn scan_to_text<T: CodeUnit>(input: &[T]) -> Option<String> {
        let mut tape = RustHtmlFragmentTape::default();
        if !scan(input, &mut tape) {
            return None;
        }
        let text_of = |start: u32, length: u32| {
            let code_units: Vec<u16> = input[start as usize..(start + length) as usize]
                .iter()
                .map(|&code_unit| code_unit.into())
                .collect();
            String::from_utf16(&code_units).unwrap()
        };
        let mut text = String::new();
        for operation in &tape.operations {
            match operation.kind {
                OpenElement | AppendVoidElement => {
                    text.push_str(if operation.kind == OpenElement { "open" } else { "void" });
                    let tag = tag_id_for_name(TAG_BYTES[operation.tag_kind as usize]);
                    text.push_str(&format!(" #{}", tag.index()));
                    let attributes = &tape.attributes[operation.start as usize..][..operation.length as usize];
                    for attribute in attributes {
                        text.push_str(&format!(
                            " {}{}={:?}{}",
                            text_of(attribute.name_start, attribute.name_length),
                            if attribute.known_name != 0 { "!" } else { "" },
                            text_of(attribute.value_start, attribute.value_length),
                            if attribute.value_is_ascii { "" } else { "~" },
                        ));
                    }
                }
                CloseElement => text.push_str("close"),
                AppendText => text.push_str(&format!(
                    "text {:?}{}",
                    text_of(operation.start, operation.length),
                    if operation.is_ascii { "" } else { "~" }
                )),
            }
            text.push('\n');
        }
        Some(text)
    }

    fn scan_str(input: &str) -> Option<String> {
        let utf16: Vec<u16> = input.encode_utf16().collect();
        let from_utf16 = scan_to_text(&utf16);
        if let Some(latin1) = utf16
            .iter()
            .map(|&code_unit| u8::try_from(code_unit).ok())
            .collect::<Option<Vec<u8>>>()
        {
            assert_eq!(scan_to_text(&latin1), from_utf16, "Latin-1 and UTF-16 input scan alike");
        }
        from_utf16
    }

    fn accepts(input: &str) -> bool {
        scan_str(input).is_some()
    }

    #[test]
    fn elements_attributes_and_text_become_operations() {
        let div = tag_id!("div").index();
        let input = tag_id!("input").index();
        assert_eq!(
            scan_str("a<div class=\"x y\" data-id='7' hidden>caf\u{E9}<input type = \"checkbox\" /></div >").unwrap(),
            format!(
                "text \"a\"\nopen #{div} class!=\"x y\" data-id=\"7\" hidden!=\"\"\ntext \"caf\u{E9}\"~\n\
                 void #{input} type!=\"checkbox\"\nclose\n"
            )
        );
        assert_eq!(scan_str("").unwrap(), "");
        assert_eq!(scan_str("x > y").unwrap(), "text \"x > y\"\n");
    }

    #[test]
    fn markup_the_tree_builder_takes_as_written_is_accepted() {
        for input in [
            "<br>",
            "<br/>",
            "<input>",
            "<li><ul><li></li></ul></li>",
            "<a><b></b></a><a></a>",
            "<div title='a<b>\"'></div>",
            "<div\n\tclass=''\x0C></div\n>",
        ] {
            assert!(accepts(input), "{input:?}");
        }
    }

    #[test]
    fn markup_for_the_general_parser_is_turned_down() {
        for input in [
            "a &amp; b",
            "a\rb",
            "a\0b",
            "<p>x</p>",
            "<my-element></my-element>",
            "<DIV></DIV>",
            "<div>",
            "</div>",
            "<div></span>",
            "<div/>",
            "</br>",
            "<div",
            "<div class='a",
            "<div class=a></div>",
            "<div Class='a'></div>",
            "<div class='a' class='b'></div>",
            "<div class='a&amp;b'></div>",
            "<div class='a\rb'></div>",
            "<div is='x'></div>",
            "<a><b><a></a></b></a>",
            "<button><button></button></button>",
            "<li><li></li></li>",
            "<ul><li><b><li></li></b></li></ul>",
            "<input type='radio'>",
            "<input type>",
            "<!-- comment -->",
        ] {
            assert!(!accepts(input), "{input:?}");
        }
        let too_deep = "<div>".repeat(MAX_OPEN_ELEMENTS + 1) + &"</div>".repeat(MAX_OPEN_ELEMENTS + 1);
        assert!(!accepts(&too_deep));
        assert!(accepts(&too_deep["<div>".len()..too_deep.len() - "</div>".len()]));
    }

    #[test]
    fn surrogates_are_taken_in_pairs_only() {
        let code_units = |text: &str, surrogate: &[u16]| {
            let mut code_units: Vec<u16> = text.encode_utf16().collect();
            let offset = code_units
                .iter()
                .position(|&code_unit| code_unit == u16::from(b'?'))
                .unwrap();
            code_units.splice(offset..offset + 1, surrogate.iter().copied());
            code_units
        };
        for text in ["a?b", "<div title='a?b'></div>"] {
            assert!(scan_to_text(&code_units(text, &[0xD83D, 0xDE00])).is_some());
            assert!(scan_to_text(&code_units(text, &[0xD83D])).is_none());
            assert!(scan_to_text(&code_units(text, &[0xDE00])).is_none());
            assert!(scan_to_text(&code_units(text, &[0xDE00, 0xD83D])).is_none());
        }
        assert_eq!(
            scan_to_text(&code_units("?", &[0xD83D, 0xDE00])).unwrap(),
            "text \"\u{1F600}\"~\n"
        );
    }

    #[test]
    fn direct_supported_tag_match_agrees_with_tag_set() {
        for &name in SUPPORTED_TAG_NAMES.iter().chain(&[
            "p",
            "table",
            "tr",
            "td",
            "script",
            "style",
            "svg",
            "template",
            "form",
            "select",
            "textarea",
            "img",
            "my-element",
            "",
        ]) {
            let expected = {
                let tag = tag_id_for_name(name.as_bytes());
                (tag != TagId::UNKNOWN && SUPPORTED_TAGS.contains(tag)).then_some(tag)
            };
            assert_eq!(
                supported_tag_for_name(name.as_bytes()).map(|(tag, _)| tag),
                expected,
                "{name}"
            );
        }
        for (expected_index, &name) in HOT_ATTRIBUTE_NAMES.iter().enumerate() {
            assert_eq!(
                hot_attribute_index(name),
                Some(expected_index),
                "{:?}",
                core::str::from_utf8(name)
            );
            assert!(known_attribute_name(name).is_some(), "{:?}", core::str::from_utf8(name));
        }
        assert!(hot_attribute_index(b"data-id").is_none());
        assert!(known_attribute_name(b"data-id").is_none());
    }
}
