/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

use smallvec::SmallVec;

#[derive(Clone, Copy)]
pub(super) enum Input<'a> {
    Latin1(&'a [u8]),
    Utf16(&'a [u16]),
}

impl Input<'_> {
    pub(super) fn len(self) -> usize {
        match self {
            Self::Latin1(input) => input.len(),
            Self::Utf16(input) => input.len(),
        }
    }

    fn code_unit_at(self, index: usize) -> u16 {
        match self {
            Self::Latin1(input) => u16::from(input[index]),
            Self::Utf16(input) => input[index],
        }
    }
}

pub(super) struct Parser<'a> {
    input: Input<'a>,
    position: usize,
}

impl<'a> Parser<'a> {
    pub(super) fn new(input: Input<'a>) -> Self {
        Self { input, position: 0 }
    }

    pub(super) fn done(&self) -> bool {
        self.position >= self.input.len()
    }

    pub(super) fn current_ascii(&self) -> Option<u8> {
        if self.done() { None } else { self.ascii_at(0) }
    }

    pub(super) fn current(&self) -> Option<u16> {
        (!self.done()).then(|| self.input.code_unit_at(self.position))
    }

    pub(super) fn ascii_at(&self, offset: usize) -> Option<u8> {
        let index = self.position.checked_add(offset)?;
        if index >= self.input.len() {
            return None;
        }
        u8::try_from(self.input.code_unit_at(index)).ok()
    }

    pub(super) fn consume(&mut self) -> u16 {
        let unit = self.input.code_unit_at(self.position);
        self.position += 1;
        unit
    }

    pub(super) fn consume_specific(&mut self, expected: &[u8]) -> bool {
        if expected
            .iter()
            .enumerate()
            .all(|(offset, expected)| self.ascii_at(offset) == Some(*expected))
        {
            self.position += expected.len();
            true
        } else {
            false
        }
    }

    pub(super) fn parse_whitespace(&mut self) {
        match self.input {
            Input::Latin1(bytes) => {
                let mut pos = self.position;
                while pos < bytes.len() && is_whitespace(bytes[pos]) {
                    pos += 1;
                }
                self.position = pos;
            }
            Input::Utf16(units) => {
                let mut pos = self.position;
                while pos < units.len() && units[pos] <= 0x20 && is_whitespace(units[pos] as u8) {
                    pos += 1;
                }
                self.position = pos;
            }
        }
    }

    pub(super) fn parse_optional_comma_whitespace(&mut self) {
        match self.input {
            Input::Latin1(bytes) => {
                let mut pos = self.position;
                let len = bytes.len();
                if pos >= len {
                    return;
                }
                let b = bytes[pos];
                if b == b',' {
                    pos += 1;
                    while pos < len && is_whitespace(bytes[pos]) {
                        pos += 1;
                    }
                    self.position = pos;
                } else if is_whitespace(b) {
                    pos += 1;
                    while pos < len && is_whitespace(bytes[pos]) {
                        pos += 1;
                    }
                    if pos < len && bytes[pos] == b',' {
                        pos += 1;
                        while pos < len && is_whitespace(bytes[pos]) {
                            pos += 1;
                        }
                    }
                    self.position = pos;
                }
            }
            Input::Utf16(_) => {
                if !self.matches_comma_whitespace() {
                    return;
                }
                if self.current_ascii() == Some(b',') {
                    self.consume();
                    self.parse_whitespace();
                } else {
                    self.parse_whitespace();
                    if self.current_ascii() == Some(b',') {
                        self.consume();
                    }
                    self.parse_whitespace();
                }
            }
        }
    }

    // https://www.w3.org/TR/SVG11/types.html#DataTypeNumber
    pub(super) fn parse_coordinate_pair(&mut self) -> Result<[f32; 2], ()> {
        let x = self.parse_number()?;
        self.parse_optional_comma_whitespace();
        let y = self.parse_number()?;
        Ok([x, y])
    }

    pub(super) fn parse_number(&mut self) -> Result<f32, ()> {
        let sign = match self.current_ascii() {
            Some(b'-') => {
                self.consume();
                -1.0
            }
            Some(b'+') => {
                self.consume();
                1.0
            }
            _ => 1.0,
        };
        Ok(sign * self.parse_nonnegative_number()?)
    }

    // https://svgwg.org/svg2-draft/types.html#syntax
    // A <number> as CSS defines it, which unlike a number in path data does not allow a dot that is not followed by a digit.
    pub(super) fn parse_css_number(&mut self) -> Result<f32, ()> {
        let start = self.position;
        let number = self.parse_number()?;
        let dot = u16::from(b'.');
        let is_digit = |unit: u16| (u16::from(b'0')..=u16::from(b'9')).contains(&unit);
        for index in start..self.position {
            if self.input.code_unit_at(index) == dot
                && (index + 1 == self.position || !is_digit(self.input.code_unit_at(index + 1)))
            {
                return Err(());
            }
        }
        Ok(number)
    }

    // https://www.w3.org/TR/SVG11/paths.html#PathDataBNF
    pub(super) fn parse_nonnegative_number(&mut self) -> Result<f32, ()> {
        // Every entry 10^0..=10^10 is exact in binary32 (5^10 = 9,765,625 < 2^24).
        const POW10_F32: [f32; 11] = [
            1.0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10,
        ];
        const MAX_EXACT_F32_MANTISSA: u64 = 16_777_215; // 2^24 - 1

        if let Input::Latin1(bytes) = self.input {
            let len = bytes.len();
            let start = self.position;
            if start >= len {
                return Err(());
            }
            let first = bytes[start];
            let starts_with_digit = first.is_ascii_digit();
            let starts_with_fraction = first == b'.' && start + 1 < len && bytes[start + 1].is_ascii_digit();
            if !starts_with_digit && !starts_with_fraction {
                return Err(());
            }
            let mut pos = start;
            let mut mantissa: u64 = 0;
            let mut fast_exact = true;
            let mut integer_digits = 0usize;
            while pos < len && bytes[pos].is_ascii_digit() {
                let d = u64::from(bytes[pos] - b'0');
                if fast_exact {
                    let next = mantissa * 10 + d;
                    if next <= MAX_EXACT_F32_MANTISSA {
                        mantissa = next;
                    } else {
                        fast_exact = false;
                    }
                }
                integer_digits += 1;
                pos += 1;
            }

            let mut fractional_digits = 0usize;
            if pos < len && bytes[pos] == b'.' {
                pos += 1;
                while pos < len && bytes[pos].is_ascii_digit() {
                    let d = u64::from(bytes[pos] - b'0');
                    if fast_exact {
                        let next = mantissa * 10 + d;
                        if next <= MAX_EXACT_F32_MANTISSA {
                            mantissa = next;
                        } else {
                            fast_exact = false;
                        }
                    }
                    fractional_digits += 1;
                    pos += 1;
                }
            }
            if integer_digits == 0 && fractional_digits == 0 {
                return Err(());
            }

            let mut has_exponent = false;
            if pos < len && matches!(bytes[pos], b'e' | b'E') {
                let exponent_start = pos;
                pos += 1;
                if pos < len && matches!(bytes[pos], b'+' | b'-') {
                    pos += 1;
                }
                let digits_start = pos;
                while pos < len && bytes[pos].is_ascii_digit() {
                    pos += 1;
                }
                if pos == digits_start {
                    pos = exponent_start;
                } else {
                    has_exponent = true;
                }
            }

            self.position = pos;
            if fast_exact && !has_exponent && fractional_digits < POW10_F32.len() {
                return Ok(if fractional_digits == 0 {
                    mantissa as f32
                } else {
                    (mantissa as f32) / POW10_F32[fractional_digits]
                });
            }
            // SAFETY: Every byte in `bytes[start..pos]` was verified to be ASCII (`0..=9`, `.`, `e`, `E`, `+`, `-`).
            let slice = unsafe { std::str::from_utf8_unchecked(&bytes[start..pos]) };
            return slice.parse::<f32>().map_err(|_| ());
        }

        let starts_with_digit = self.current_ascii().is_some_and(|unit| unit.is_ascii_digit());
        let starts_with_fraction =
            self.current_ascii() == Some(b'.') && self.ascii_at(1).is_some_and(|unit| unit.is_ascii_digit());
        if !starts_with_digit && !starts_with_fraction {
            return Err(());
        }
        let start = self.position;

        let mut integer_digits = 0;
        while self.current_ascii().is_some_and(|unit| unit.is_ascii_digit()) {
            integer_digits += 1;
            self.consume();
        }

        let mut fractional_digits = 0;
        if self.current_ascii() == Some(b'.') {
            self.consume();
            while self.current_ascii().is_some_and(|unit| unit.is_ascii_digit()) {
                fractional_digits += 1;
                self.consume();
            }
        }
        if integer_digits == 0 && fractional_digits == 0 {
            return Err(());
        }

        if matches!(self.current_ascii(), Some(b'e' | b'E')) {
            let exponent_start = self.position;
            self.consume();
            if matches!(self.current_ascii(), Some(b'+' | b'-')) {
                self.consume();
            }
            let digits_start = self.position;
            while self.current_ascii().is_some_and(|unit| unit.is_ascii_digit()) {
                self.consume();
            }
            if self.position == digits_start {
                self.position = exponent_start;
            }
        }

        let number = self.ascii_range(start, self.position)?;
        std::str::from_utf8(&number)
            .map_err(|_| ())?
            .parse::<f32>()
            .map_err(|_| ())
    }

    pub(super) fn parse_integer(&mut self) -> Result<i32, ()> {
        if let Input::Latin1(bytes) = self.input {
            let len = bytes.len();
            let start = self.position;
            let mut pos = start;
            if pos < len && matches!(bytes[pos], b'+' | b'-') {
                pos += 1;
            }
            let digits_start = pos;
            while pos < len && bytes[pos].is_ascii_digit() {
                pos += 1;
            }
            if pos == digits_start {
                return Err(());
            }
            self.position = pos;
            // SAFETY: Every byte in `bytes[start..pos]` was verified to be ASCII (`+`, `-`, `0..=9`).
            let slice = unsafe { std::str::from_utf8_unchecked(&bytes[start..pos]) };
            return slice.parse::<i32>().map_err(|_| ());
        }
        let start = self.position;
        if matches!(self.current_ascii(), Some(b'+' | b'-')) {
            self.consume();
        }
        let digits_start = self.position;
        while self.current_ascii().is_some_and(|unit| unit.is_ascii_digit()) {
            self.consume();
        }
        if self.position == digits_start {
            return Err(());
        }
        let integer = self.ascii_range(start, self.position)?;
        std::str::from_utf8(&integer)
            .map_err(|_| ())?
            .parse::<i32>()
            .map_err(|_| ())
    }

    pub(super) fn matches_coordinate(&self) -> bool {
        let Some(mut unit) = self.current_ascii() else {
            return false;
        };
        let mut offset = 0;
        if matches!(unit, b'+' | b'-') {
            offset += 1;
            let Some(next) = self.ascii_at(offset) else {
                return false;
            };
            unit = next;
        }
        if unit == b'.' {
            offset += 1;
            let Some(next) = self.ascii_at(offset) else {
                return false;
            };
            unit = next;
        }
        unit.is_ascii_digit()
    }

    pub(super) fn matches_comma_whitespace(&self) -> bool {
        self.current_ascii()
            .is_some_and(|unit| unit == b',' || is_whitespace(unit))
    }

    fn ascii_range(&self, start: usize, end: usize) -> Result<SmallVec<[u8; 32]>, ()> {
        let mut bytes = SmallVec::with_capacity(end - start);
        for index in start..end {
            bytes.push(u8::try_from(self.input.code_unit_at(index)).map_err(|_| ())?);
        }
        Ok(bytes)
    }
}

fn is_whitespace(unit: u8) -> bool {
    matches!(unit, b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_latin1_number_parsing_matches_std_parse_bit_for_bit() {
        let cases = [
            "0",
            "-0",
            "0.0",
            "-0.0",
            ".5",
            "-.5",
            "1.",
            "123",
            "123.456",
            "-987.6543",
            "0.000000123",
            "0.0000000001",
            "0.00000000001",
            "16777215",
            "16777216",
            "16777217",
            "9999999.5",
            "1234567.89",
            "1e-5",
            "2.5E+3",
            "3.1415926",
            "3.1415926535",
            "0000123.45",
        ];
        for case in cases {
            let expected: f32 = case.parse().unwrap();
            let mut latin1 = Parser::new(Input::Latin1(case.as_bytes()));
            let actual_latin1 = latin1.parse_number().unwrap();
            assert!(latin1.done(), "unconsumed trailing bytes for {case:?}");
            assert_eq!(
                actual_latin1.to_bits(),
                expected.to_bits(),
                "latin1 mismatch for {case:?}: got {actual_latin1} vs {expected}"
            );
            let utf16_units: Vec<u16> = case.encode_utf16().collect();
            let mut utf16 = Parser::new(Input::Utf16(&utf16_units));
            let actual_utf16 = utf16.parse_number().unwrap();
            assert_eq!(
                actual_utf16.to_bits(),
                expected.to_bits(),
                "utf16 mismatch for {case:?}: got {actual_utf16} vs {expected}"
            );
        }
    }
}
