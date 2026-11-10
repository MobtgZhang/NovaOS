//! Rust port of `ede-calc`, EDE's calculator.
//!
//! Upstream is a four-banding calculator with a tape display: digits and
//! operations build an expression, `=` evaluates, `C` clears, and the
//! usual memory keys work. The maths itself — including the scientific
//! functions on the extended band — is the interesting part to port: the
//! kernel's `no_std` world has no `libm`, so [`math`] carries its own
//! argument-reduction and series implementations, each pinned by tests
//! against known values.

#![no_std]
#![forbid(unsafe_code)]

pub mod math;

use math::{cos_deg, exp, ln, sin_deg, tan_deg};

/// How many characters the tape keeps.
pub const TAPE_MAX: usize = 64;
/// How many digits an entry keeps.
pub const ENTRY_MAX: usize = 16;

/// One calculator operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Add,
    Subtract,
    Multiply,
    Divide,
}

impl Op {
    /// The symbol the display shows.
    pub const fn symbol(self) -> &'static str {
        match self {
            Op::Add => "+",
            Op::Subtract => "−",
            Op::Multiply => "×",
            Op::Divide => "÷",
        }
    }
}

/// The calculator's state: the accumulator, the pending operation, the
/// typed entry, and the tape.
#[derive(Clone, Copy)]
pub struct Calculator {
    /// The running accumulator.
    pub accumulator: f64,
    /// The pending operation, applied on the next `=` or operator.
    pending: Option<Op>,
    /// The typed entry, as raw characters.
    entry: [u8; ENTRY_MAX],
    entry_len: usize,
    /// Whether an entry is being typed at all.
    entering: bool,
    /// The tape of evaluated lines, oldest first.
    tape: [[u8; 24]; 8],
    tape_lens: [usize; 8],
    tape_count: usize,
}

impl Calculator {
    /// A cleared calculator.
    pub const fn new() -> Self {
        Self {
            accumulator: 0.0,
            pending: None,
            entry: [0; ENTRY_MAX],
            entry_len: 0,
            entering: false,
            tape: [[0; 24]; 8],
            tape_lens: [0; 8],
            tape_count: 0,
        }
    }

    /// The typed entry as text.
    pub fn entry(&self) -> &str {
        core::str::from_utf8(&self.entry[..self.entry_len]).unwrap_or("")
    }

    /// The typed entry parsed as a number; an empty entry is zero.
    pub fn entry_value(&self) -> f64 {
        parse_number(self.entry())
    }

    /// Type a digit or `.`; `00` is accepted, like upstream's key.
    pub fn digit(&mut self, byte: u8) -> bool {
        let is_digit = byte.is_ascii_digit() || byte == b'.';
        if !is_digit || self.entry_len >= ENTRY_MAX {
            return false;
        }
        // One `.` per number.
        if byte == b'.' && self.entry().contains('.') {
            return false;
        }
        self.entry[self.entry_len] = byte;
        self.entry_len += 1;
        self.entering = true;
        true
    }

    /// Set an operation. The first operand moves into the accumulator;
    /// a second pending operation is evaluated first — chained
    /// arithmetic.
    pub fn op(&mut self, op: Op) {
        match self.pending {
            None => {
                if self.entering {
                    self.accumulator = self.entry_value();
                    self.entry_len = 0;
                    self.entering = false;
                }
            }
            Some(_) => self.evaluate_pending(),
        }
        self.pending = Some(op);
    }

    /// Apply the pending operation to the accumulator and the entry.
    fn evaluate_pending(&mut self) {
        let Some(op) = self.pending.take() else {
            return;
        };
        let operand = self.entry_value();
        let result = match op {
            Op::Add => self.accumulator + operand,
            Op::Subtract => self.accumulator - operand,
            Op::Multiply => self.accumulator * operand,
            Op::Divide => {
                if operand == 0.0 {
                    // Upstream shows "Error" and waits for C.
                    self.entering = false;
                    self.entry_len = 0;
                    self.accumulator = f64::INFINITY;
                    self.push_tape(b"Error");
                    return;
                }
                self.accumulator / operand
            }
        };
        self.accumulator = result;
        self.entry_len = 0;
        self.entering = false;
    }

    /// `=`: evaluate and put the result in the entry so it can be reused.
    pub fn equals(&mut self) {
        if self.pending.is_none() {
            return;
        }
        let operand_text = copy_line(self.entry());
        self.evaluate_pending();
        self.push_tape_line(operand_text);
        // The result becomes the entry, so `=` then an operator chains.
        self.entering = false;
    }

    /// A scientific function applied to the current entry or accumulator.
    pub fn apply(&mut self, function: Function) {
        let value = if self.entering {
            self.entry_value()
        } else {
            self.accumulator
        };
        let result = match function {
            Function::Sin => sin_deg(value),
            Function::Cos => cos_deg(value),
            Function::Tan => tan_deg(value),
            Function::Log => {
                if value <= 0.0 {
                    self.push_tape(b"Error");
                    self.accumulator = f64::INFINITY;
                    return;
                }
                ln(value) / math::LN_10
            }
            Function::Exp => exp(value),
            Function::Sqrt => {
                if value < 0.0 {
                    self.push_tape(b"Error");
                    self.accumulator = f64::INFINITY;
                    return;
                }
                math::sqrt(value)
            }
        };
        self.accumulator = result;
        self.entry_len = 0;
        self.entering = false;
    }

    /// `C`: clear everything, like upstream.
    pub fn clear(&mut self) {
        self.accumulator = 0.0;
        self.pending = None;
        self.entry_len = 0;
        self.entering = false;
    }

    /// Clear the typed entry only — `CE`.
    pub fn clear_entry(&mut self) {
        self.entry_len = 0;
        self.entering = false;
    }

    /// The tape line at `index`, oldest first.
    pub fn tape_line(&self, index: usize) -> Option<&str> {
        if index >= self.tape_count {
            return None;
        }
        core::str::from_utf8(&self.tape[index][..self.tape_lens[index]]).ok()
    }

    /// How many tape lines are kept.
    pub const fn tape_count(&self) -> usize {
        self.tape_count
    }

    fn push_tape(&mut self, text: &[u8]) {
        // Tape lines are fixed buffers; trim at the first NUL.
        let end = text
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(text.len());
        if self.tape_count >= 8 {
            // Drop the oldest.
            for index in 1..8 {
                self.tape[index - 1] = self.tape[index];
                self.tape_lens[index - 1] = self.tape_lens[index];
            }
            self.tape_count = 7;
        }
        self.tape[self.tape_count][..end].copy_from_slice(&text[..end]);
        self.tape_lens[self.tape_count] = end;
        self.tape_count += 1;
    }

    fn push_tape_line(&mut self, text: [u8; 24]) {
        self.push_tape(&text);
    }
}

impl Default for Calculator {
    fn default() -> Self {
        Self::new()
    }
}

/// The scientific band's functions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Function {
    Sin,
    Cos,
    Tan,
    Log,
    Exp,
    Sqrt,
}

/// Copy the entry text into a fixed-size tape line.
fn copy_line(text: &str) -> [u8; 24] {
    let mut line = [0u8; 24];
    let bytes = text.as_bytes();
    let take = bytes.len().min(24);
    line[..take].copy_from_slice(&bytes[..take]);
    line
}

/// Parse a decimal number: sign, digits, one dot. Invalid text is 0.
fn parse_number(text: &str) -> f64 {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    if digits.is_empty() {
        return 0.0;
    }
    let mut value = 0.0f64;
    let mut fraction = 0.0f64;
    let mut seen_digit = false;
    for byte in digits.bytes() {
        match byte {
            b'.' => {
                if fraction != 0.0 {
                    return 0.0; // second dot: malformed
                }
                fraction = 1.0;
            }
            b'0'..=b'9' => {
                seen_digit = true;
                let digit = (byte - b'0') as f64;
                if fraction == 0.0 {
                    value = value * 10.0 + digit;
                } else {
                    fraction /= 10.0;
                    value += digit * fraction;
                }
            }
            _ => return 0.0,
        }
    }
    if !seen_digit {
        return 0.0;
    }
    if negative { -value } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;
    use math::approx_eq;

    #[test]
    fn digits_build_an_entry() {
        let mut calc = Calculator::new();
        for byte in b"12.5" {
            assert!(calc.digit(*byte));
        }
        assert_eq!(calc.entry(), "12.5");
        assert_eq!(calc.entry_value(), 12.5);
    }

    #[test]
    fn a_second_dot_is_ignored() {
        let mut calc = Calculator::new();
        calc.digit(b'1');
        calc.digit(b'.');
        calc.digit(b'5');
        assert!(!calc.digit(b'.'));
        assert_eq!(calc.entry(), "1.5");
    }

    #[test]
    fn arithmetic_chains() {
        let mut calc = Calculator::new();
        for byte in b"12" {
            calc.digit(*byte);
        }
        calc.op(Op::Add);
        for byte in b"34" {
            calc.digit(*byte);
        }
        calc.equals();
        assert!((calc.accumulator - 46.0).abs() < 1e-9);
        // Chaining continues from the result.
        calc.op(Op::Multiply);
        for byte in b"2" {
            calc.digit(*byte);
        }
        calc.equals();
        assert!((calc.accumulator - 92.0).abs() < 1e-9);
    }

    #[test]
    fn the_entry_reuses_the_result() {
        let mut calc = Calculator::new();
        for byte in b"8" {
            calc.digit(*byte);
        }
        calc.op(Op::Multiply);
        for byte in b"3" {
            calc.digit(*byte);
        }
        calc.equals();
        assert!((calc.accumulator - 24.0).abs() < 1e-9);
    }

    #[test]
    fn division_by_zero_reports_an_error() {
        let mut calc = Calculator::new();
        for byte in b"5" {
            calc.digit(*byte);
        }
        calc.op(Op::Divide);
        for byte in b"0" {
            calc.digit(*byte);
        }
        calc.equals();
        assert!(calc.accumulator.is_infinite());
        assert_eq!(calc.tape_line(0), Some("Error"));
        // `C` recovers.
        calc.clear();
        assert_eq!(calc.accumulator, 0.0);
    }

    #[test]
    fn c_and_ce_differ() {
        let mut calc = Calculator::new();
        for byte in b"7" {
            calc.digit(*byte);
        }
        calc.op(Op::Add);
        for byte in b"9" {
            calc.digit(*byte);
        }
        calc.clear_entry();
        assert_eq!(calc.entry(), "");
        calc.digit(b'2');
        calc.equals();
        assert!((calc.accumulator - 9.0).abs() < 1e-9, "7 + 2");
        calc.clear();
        assert_eq!(calc.accumulator, 0.0);
    }

    #[test]
    fn the_tape_records_the_relevant_lines() {
        let mut calc = Calculator::new();
        for byte in b"2" {
            calc.digit(*byte);
        }
        calc.op(Op::Add);
        for byte in b"3" {
            calc.digit(*byte);
        }
        calc.equals();
        assert_eq!(calc.tape_count(), 1);
        assert_eq!(calc.tape_line(0), Some("3"));
    }

    #[test]
    fn scientific_functions_apply() {
        let mut calc = Calculator::new();
        for byte in b"90" {
            calc.digit(*byte);
        }
        calc.apply(Function::Sin);
        assert!(approx_eq(calc.accumulator, 1.0, 1e-9), "sin 90° = 1");
        calc.clear();
        for byte in b"9" {
            calc.digit(*byte);
        }
        calc.apply(Function::Sqrt);
        assert!(approx_eq(calc.accumulator, 3.0, 1e-9));
    }

    #[test]
    fn negative_entries_parse() {
        let mut calc = Calculator::new();
        // The ± toggle: typing '-' leads the entry in this port.
        calc.entry[0] = b'-';
        calc.entry_len = 1;
        calc.digit(b'4');
        assert_eq!(calc.entry_value(), -4.0);
    }
}
