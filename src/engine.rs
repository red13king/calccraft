//! Calculator engine: a port of the Windows Calculator's `CCalcEngine`
//! (`CalcManager/CEngine/scicomm.cpp`) and `CHistoryCollector` (`History.cpp`),
//! independent of the user interface.
//!
//! Standard mode applies operations immediately, without precedence:
//! `2 + 3 × 4 =` gives 20. With precedence on (`Calculator::with_precedence`,
//! for the Scientific mode) the same engine keeps a stack of pending operations
//! and parentheses: `2 + 3 × 4 =` gives 14. Numbers are exact fractions
//! (`BigRational`), so `0.1 + 0.2 − 0.3` is exactly 0 and `1 ÷ 3 × 3` is exactly 1.
//!
//! The state and the order of the steps follow the Windows source closely, so
//! the display and the expression line behave like Windows, quirks included.
//! The names of the Windows fields are given in the comments.
//!
//! `Calculator::programmer()` is Windows' integer mode for the Programmer mode: the same
//! engine with precedence, a radix and a word size (see `integer.rs`). The Scientific
//! mode's functions are in `math.rs`.

mod integer;
mod math;

use std::cmp::Ordering;

use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{Signed, ToPrimitive, Zero};

use integer::IntegerMode;
pub use integer::{Radix, WordSize};
pub use math::Angle;

pub type Number = BigRational;

/// How many digits can be typed and how many significant digits the display shows in
/// the Standard mode (`StandardModePrecision`).
const MAX_DIGITS: i64 = 16;
/// The same in the Scientific mode (`ScientificModePrecision`).
const SCIENTIFIC_DIGITS: i64 = 32;
/// The longest exponent that can be typed after `exp` (`C_EXP_MAX_DIGITS`).
const MAX_EXPONENT_DIGITS: usize = 4;
/// How many zeros after the decimal point plain notation allows when the number
/// does not fit completely (0.0033… stays plain, 0.00033… becomes 3.3…e-4).
const MAX_ZEROS_AFTER_DECIMAL: i64 = 2;
/// When a result's denominator grows beyond this many bits, the result is rounded
/// to `INTERNAL_DIGITS` significant digits so fractions don't grow without bound.
const MAX_DENOM_BITS: u64 = 256;
const INTERNAL_DIGITS: i64 = 40;
/// Largest decimal exponent; anything beyond it is "Overflow" (as in Windows).
const MAX_EXPONENT: i64 = 9999;
const HISTORY_LIMIT: usize = 20;
const MEMORY_LIMIT: usize = 100;
/// Deepest nesting of pending operations and of parentheses (`MAXPRECDEPTH`).
const MAX_DEPTH: usize = 25;
/// Paste limits from Windows' `CopyPasteManager`.
const MAX_PASTE_LENGTH: usize = 512;
const MAX_PASTE_OPERANDS: usize = 100;
const MAX_PASTE_EXPONENT_DIGITS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Add,
    Subtract,
    Multiply,
    Divide,
    /// The Programmer mode's operators: the remainder (`%`), the bitwise operators, and
    /// the shifts (`Lsh` and the arithmetic and logical `Rsh`).
    Mod,
    And,
    Or,
    Xor,
    Nand,
    Nor,
    LeftShift,
    RightShift,
    RightShiftLogical,
    /// The Scientific mode's `xʸ`, `ʸ√x` (the left operand's yth root) and `logᵧx` (the
    /// left operand's logarithm to the base of the right one).
    Power,
    Root,
    LogBase,
}

impl Op {
    /// The symbol in the expression line: Windows' engine strings, where
    /// subtraction is a plain hyphen. They aren't translated.
    fn symbol(self, integer: bool) -> &'static str {
        match self {
            Op::Add => "+",
            Op::Subtract => "-",
            Op::Multiply => "×",
            Op::Divide => "÷",
            // `SIDS_PROGRAMMER_MOD`; the Scientific mode writes `Mod` (`SIDS_MOD`).
            Op::Mod if integer => "%",
            Op::Mod => "Mod",
            Op::And => "AND",
            Op::Or => "OR",
            Op::Xor => "XOR",
            Op::Nand => "NAND",
            Op::Nor => "NOR",
            Op::LeftShift => "Lsh",
            Op::RightShift | Op::RightShiftLogical => "Rsh",
            Op::Power => "^",
            Op::Root => "yroot",
            Op::LogBase => "log base",
        }
    }

    /// Higher binds tighter (`NPrecedenceOfOp`). `OR` and `XOR` rank with no
    /// operator at all.
    fn precedence(self) -> u8 {
        match self {
            Op::Or | Op::Xor => 0,
            Op::And | Op::Nand | Op::Nor => 1,
            Op::Add | Op::Subtract => 2,
            Op::Multiply
            | Op::Divide
            | Op::Mod
            | Op::LeftShift
            | Op::RightShift
            | Op::RightShiftLogical => 3,
            Op::Power | Op::Root | Op::LogBase => 4,
        }
    }

    /// The Programmer mode's operators, which the other modes don't have.
    fn is_integer(self) -> bool {
        matches!(
            self,
            Op::And
                | Op::Or
                | Op::Xor
                | Op::Nand
                | Op::Nor
                | Op::LeftShift
                | Op::RightShift
                | Op::RightShiftLogical
        )
    }

    /// The Scientific mode's operators.
    fn is_scientific(self) -> bool {
        matches!(self, Op::Power | Op::Root | Op::LogBase)
    }
}

/// Precedence of a pending operation; no operation counts as the lowest (0).
fn precedence(op: Option<Op>) -> u8 {
    op.map_or(0, Op::precedence)
}

/// Unary functions applied to the current number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Func {
    Reciprocal,
    Square,
    SquareRoot,
    Negate,
    /// The Programmer mode's functions: `NOT`, and rotations by one bit, circular or
    /// through the carry bit.
    Not,
    RotateLeft,
    RotateRight,
    RotateLeftCarry,
    RotateRightCarry,
    /// The Scientific mode's functions: `x³`, `∛x`, `10ˣ`, `2ˣ`, `eˣ`, `log`, `ln`, `n!`,
    /// `|x|`, `⌊x⌋`, `⌈x⌉`, `→dms`, `→deg`, and the trigonometric functions.
    Cube,
    CubeRoot,
    TenToX,
    TwoToX,
    EToX,
    Log,
    Ln,
    Factorial,
    Abs,
    Floor,
    Ceil,
    Dms,
    Degrees,
    Trig(Trig),
}

/// A trigonometric function, or its inverse (`2nd` in the Trigonometry menu), or a
/// hyperbolic one (`hyp`), or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trig {
    pub function: TrigFunction,
    pub inverse: bool,
    pub hyperbolic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrigFunction {
    Sin,
    Cos,
    Tan,
    Sec,
    Csc,
    Cot,
}

impl TrigFunction {
    pub const ALL: [TrigFunction; 6] = [
        TrigFunction::Sin,
        TrigFunction::Cos,
        TrigFunction::Tan,
        TrigFunction::Sec,
        TrigFunction::Csc,
        TrigFunction::Cot,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TrigFunction::Sin => "sin",
            TrigFunction::Cos => "cos",
            TrigFunction::Tan => "tan",
            TrigFunction::Sec => "sec",
            TrigFunction::Csc => "csc",
            TrigFunction::Cot => "cot",
        }
    }
}

impl Func {
    /// The function's name in the expression line, as in `sqr(5)`: Windows' engine
    /// strings (`OpCodeToUnaryString`), which aren't translated. The trigonometric
    /// functions show the angle unit as a subscript: `sin₀` in degrees, `sinᵣ` in
    /// radians and `sin₉` in gradians.
    fn name(self, angle: Angle) -> String {
        let name = match self {
            Func::Reciprocal => "1/",
            Func::Square => "sqr",
            Func::SquareRoot => "√",
            Func::Negate => "negate",
            Func::Not => "NOT",
            Func::RotateLeft | Func::RotateLeftCarry => "RoL",
            Func::RotateRight | Func::RotateRightCarry => "RoR",
            Func::Cube => "cube",
            Func::CubeRoot => "cuberoot",
            Func::TenToX => "10^",
            Func::TwoToX => "2^",
            Func::EToX => "e^",
            Func::Log => "log",
            Func::Ln => "ln",
            Func::Factorial => "fact",
            Func::Abs => "abs",
            Func::Floor => "floor",
            Func::Ceil => "ceil",
            Func::Dms => "dms",
            Func::Degrees => "degrees",
            Func::Trig(trig) => {
                let mut name = trig.function.label().to_string();
                if trig.hyperbolic {
                    name.push('h');
                } else {
                    name.push(match angle {
                        Angle::Degrees => '₀',
                        Angle::Radians => 'ᵣ',
                        Angle::Gradians => '₉',
                    });
                }
                if trig.inverse {
                    name.push_str("⁻¹");
                }
                return name;
            }
        };
        name.to_string()
    }

    /// Whether the function exists only in the Programmer mode.
    fn is_integer(self) -> bool {
        matches!(
            self,
            Func::Not
                | Func::RotateLeft
                | Func::RotateRight
                | Func::RotateLeftCarry
                | Func::RotateRightCarry
        )
    }

    /// Whether the function exists only in the Scientific mode.
    fn is_scientific(self) -> bool {
        !self.is_integer()
            && !matches!(
                self,
                Func::Reciprocal | Func::Square | Func::SquareRoot | Func::Negate
            )
    }

    fn apply(self, v: &Number, angle: Angle) -> Result<Number, CalcError> {
        let ten = || Number::from_integer(10.into());
        let two = || Number::from_integer(2.into());
        let result = match self {
            Func::Reciprocal => math::invert(v)?,
            Func::Square => v * v,
            Func::SquareRoot => return sqrt(v),
            Func::Negate => -v,
            Func::Cube => v * v * v,
            Func::CubeRoot => math::root(v, &Number::from_integer(3.into()))?,
            Func::TenToX => math::power(&ten(), v)?,
            Func::TwoToX => math::power(&two(), v)?,
            Func::EToX => math::exp(v)?,
            Func::Log => math::log10(v)?,
            Func::Ln => math::ln(v)?,
            Func::Factorial => math::factorial(v)?,
            Func::Abs => v.abs(),
            Func::Floor => math::floor(v),
            Func::Ceil => math::ceil(v),
            Func::Dms => math::dms(v, false),
            Func::Degrees => math::dms(v, true),
            Func::Trig(trig) => trig_function(trig, v, angle)?,
            // The integer functions need the word size; `Calculator::unary` does them.
            _ => v.clone(),
        };
        normalize(result)
    }
}

/// A trigonometric function as `SciCalcFunctions` computes it: the reciprocal ones as
/// 1/cos, 1/sin and 1/tan, and their inverses from the reciprocal of the value.
fn trig_function(trig: Trig, v: &Number, angle: Angle) -> Result<Number, CalcError> {
    use TrigFunction::*;
    let reciprocal = matches!(trig.function, Sec | Csc | Cot);
    let base = match trig.function {
        Sin | Csc => Sin,
        Cos | Sec => Cos,
        Tan | Cot => Tan,
    };
    let argument = if reciprocal && trig.inverse {
        math::invert(v)?
    } else {
        v.clone()
    };
    let result = match (base, trig.inverse, trig.hyperbolic) {
        (Sin, false, false) => math::sin(&argument, angle)?,
        (Cos, false, false) => math::cos(&argument, angle)?,
        (Tan, false, false) => math::tan(&argument, angle)?,
        (Sin, true, false) => math::asin(&argument, angle)?,
        (Cos, true, false) => math::acos(&argument, angle)?,
        (Tan, true, false) => math::atan(&argument, angle)?,
        (Sin, false, true) => math::sinh(&argument)?,
        (Cos, false, true) => math::cosh(&argument)?,
        (Tan, false, true) => math::tanh(&argument)?,
        (Sin, true, true) => math::asinh(&argument)?,
        (Cos, true, true) => math::acosh(&argument)?,
        (_, true, true) => math::atanh(&argument)?,
        _ => unreachable!("the reciprocal functions map to sin, cos and tan"),
    };
    if reciprocal && !trig.inverse {
        math::invert(&result)
    } else {
        Ok(result)
    }
}

/// Every command a button or a key can send to the calculator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Digit(u8),
    Decimal,
    Operator(Op),
    Function(Func),
    Percent,
    Equals,
    ClearEntry,
    Clear,
    Backspace,
    /// Parentheses work only with precedence on (Scientific mode).
    OpenParen,
    CloseParen,
    MemoryClear,
    MemoryRecall,
    MemoryAdd,
    MemorySubtract,
    MemoryStore,
    /// The Programmer mode's bit toggling keypad: flips bit 0 to 63 (`IDC_BINPOS0` …).
    ToggleBit(u8),
    /// The Programmer mode's radix (`IDM_HEX` …) and word size (`IDM_QWORD` …).
    Radix(Radix),
    WordSize(WordSize),
    /// The Scientific mode's constants π and e and a random number (`IDC_PI`,
    /// `IDC_EULER`, `IDC_RAND`).
    Pi,
    Euler,
    Random,
    /// `exp`: the typed number goes on with an exponent (`IDC_EXP`). Paste uses it in the
    /// Standard mode too.
    Exponent,
    /// `F-E`: scientific notation on or off (`IDC_FE`).
    ToggleNotation,
    /// DEG, RAD or GRAD (`IDM_DEG` …).
    Angle(Angle),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalcError {
    DivideByZero,
    Undefined,
    InvalidInput,
    Overflow,
    /// A shift by the word size or more (`CALC_E_NORESULT`).
    NoResult,
}

impl CalcError {
    /// The English message; the UI shows the translation of `resource_id`.
    pub fn message(self) -> &'static str {
        match self {
            CalcError::DivideByZero => "Cannot divide by zero",
            CalcError::Undefined => "Result is undefined",
            CalcError::InvalidInput => "Invalid input",
            CalcError::Overflow => "Overflow",
            CalcError::NoResult => "Result not defined",
        }
    }

    /// The message's id in Windows' `CEngineStrings.resw` (`EngineStrings.h`).
    pub fn resource_id(self) -> u16 {
        match self {
            CalcError::DivideByZero => 99,
            CalcError::InvalidInput => 100,
            CalcError::Undefined => 101,
            CalcError::Overflow => 107,
            CalcError::NoResult => 108,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub expression: String,
    pub result: Number,
    /// The result as the display showed it (Windows keeps this string): with the mode's
    /// digits and notation, and thousands separators.
    pub text: String,
}

/// The kind of the previous command (`m_nLastCom`); many decisions depend on it.
/// `⌫` and the memory buttons except MR are "GUI settings" in Windows and
/// don't count as commands here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    None,
    Digit,
    Point,
    Operator(Op),
    Unary,
    Sign,
    OpenParen,
    CloseParen,
    Equals,
    Clear,
    ClearEntry,
    Recall,
    BitToggle,
    /// π, e and a random number, which are like typing a number.
    Constant,
    /// `→deg`, which Windows' `IsUnaryOpCode` doesn't count as a function.
    Degrees,
}

impl Key {
    fn of(command: Command) -> Option<Key> {
        Some(match command {
            Command::Digit(_) => Key::Digit,
            Command::Decimal => Key::Point,
            Command::Operator(op) => Key::Operator(op),
            Command::Function(Func::Negate) => Key::Sign,
            Command::Function(Func::Degrees) => Key::Degrees,
            Command::Function(_) | Command::Percent => Key::Unary,
            Command::Equals => Key::Equals,
            Command::ClearEntry => Key::ClearEntry,
            Command::Clear => Key::Clear,
            Command::OpenParen => Key::OpenParen,
            Command::CloseParen => Key::CloseParen,
            Command::MemoryRecall => Key::Recall,
            Command::ToggleBit(_) => Key::BitToggle,
            Command::Pi | Command::Euler | Command::Random => Key::Constant,
            // Settings, not commands (`IsGuiSettingOpCode`).
            Command::Backspace
            | Command::MemoryClear
            | Command::MemoryAdd
            | Command::MemorySubtract
            | Command::MemoryStore
            | Command::Radix(_)
            | Command::WordSize(_)
            | Command::Exponent
            | Command::ToggleNotation
            | Command::Angle(_) => return None,
        })
    }
}

/// The exponent typed after `exp`.
#[derive(Clone, Debug, Default)]
struct Exponent {
    negative: bool,
    digits: String,
}

/// The number being typed (`CalcInput`).
#[derive(Clone, Debug, Default)]
struct Input {
    negative: bool,
    /// Digits (`0`–`9`, and `A`–`F` in the Programmer mode) and at most one decimal
    /// point; empty means 0.
    digits: String,
    /// The exponent, once `exp` was pressed (`m_hasExponent`).
    exponent: Option<Exponent>,
}

impl Input {
    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Adds a digit unless the limit is reached: `max` digits (16 in the Standard mode and
    /// 32 in the Scientific mode), or in the Programmer mode as many as the word holds,
    /// or 4 in the exponent. A leading zero is never stored, and `0.` doesn't count its
    /// zero against the limit.
    fn try_add_digit(&mut self, digit: u8, integer: Option<&IntegerMode>, max: usize) -> bool {
        if let Some(exponent) = &mut self.exponent {
            if exponent.digits.is_empty() && digit == 0 {
                return true;
            }
            if exponent.digits.len() >= MAX_EXPONENT_DIGITS {
                return false;
            }
            exponent.digits.push(char::from(b'0' + digit));
            return true;
        }
        if let Some(mode) = integer {
            if self.digits.is_empty() && digit == 0 {
                return true;
            }
            let c = char::from_digit(digit.into(), 16)
                .unwrap_or('0')
                .to_ascii_uppercase();
            if !mode.digit_fits(&self.digits, c, self.negative) {
                return false;
            }
            self.digits.push(c);
            return true;
        }
        let mut max = max;
        if self.digits.contains('.') {
            max += 1;
        }
        if self.digits.starts_with('0') {
            max += 1;
        }
        if self.digits.is_empty() && digit == 0 {
            return true;
        }
        if self.digits.len() < max {
            self.digits.push(char::from(b'0' + digit));
            true
        } else {
            false
        }
    }

    fn try_add_point(&mut self) -> bool {
        if self.digits.contains('.') || self.exponent.is_some() {
            return false;
        }
        if self.digits.is_empty() {
            self.digits.push('0');
        }
        self.digits.push('.');
        true
    }

    /// Starts the exponent, adding a decimal point first if there is none, as Windows
    /// does for compatibility (`5` becomes `5.e+0`).
    fn try_begin_exponent(&mut self) -> bool {
        self.try_add_point();
        if self.exponent.is_some() {
            return false;
        }
        self.exponent = Some(Exponent::default());
        true
    }

    /// Flips the sign, of the exponent once there is one; an empty input (0) stays
    /// positive. In the Programmer mode a negative number that wouldn't fit as a
    /// positive one keeps its sign.
    fn toggle_sign(&mut self, integer: Option<&IntegerMode>) -> bool {
        if self.digits.is_empty() {
            self.negative = false;
            if let Some(exponent) = &mut self.exponent {
                exponent.negative = false;
            }
            return true;
        }
        if let Some(exponent) = &mut self.exponent {
            exponent.negative = !exponent.negative;
            return true;
        }
        if integer.is_some_and(|mode| !mode.sign_toggles(&self.digits, self.negative)) {
            return false;
        }
        self.negative = !self.negative;
        true
    }

    /// Removes the last digit: of the exponent, then the exponent itself (its sign goes
    /// with its last digit), then the digits of the number.
    fn backspace(&mut self) {
        if let Some(exponent) = &mut self.exponent {
            if exponent.digits.pop().is_some() {
                if exponent.digits.is_empty() {
                    exponent.negative = false;
                }
            } else {
                self.exponent = None;
            }
            return;
        }
        self.digits.pop();
        if self.digits == "0" {
            self.digits.pop();
        }
        if self.digits.is_empty() {
            self.clear();
        }
    }

    /// Whether nothing has been typed (`CalcInput::IsEmpty`).
    fn is_empty(&self) -> bool {
        self.digits.is_empty() && self.exponent.is_none()
    }

    /// The typed number as Windows writes it: `-12.5`, or with an exponent `1.5e+3`.
    fn text(&self) -> String {
        let sign = if self.negative { "-" } else { "" };
        let digits = if self.digits.is_empty() {
            "0"
        } else {
            &self.digits
        };
        format!("{sign}{digits}{}", self.exponent_text())
    }

    /// `e+3`, `e-12` or `e+0` after `exp`, with the decimal point it needs.
    fn exponent_text(&self) -> String {
        let Some(exponent) = &self.exponent else {
            return String::new();
        };
        let point = if self.digits.contains('.') { "" } else { "." };
        let sign = if exponent.negative { '-' } else { '+' };
        let digits = if exponent.digits.is_empty() {
            "0"
        } else {
            &exponent.digits
        };
        format!("{point}e{sign}{digits}")
    }

    /// The typed number's value, in the Programmer mode's radix (`CalcInput::ToRational`).
    fn value(&self, integer: Option<&IntegerMode>) -> Number {
        match integer {
            Some(mode) => {
                let magnitude = BigInt::parse_bytes(self.digits.as_bytes(), mode.radix.base())
                    .unwrap_or_default();
                let v = Number::from_integer(magnitude);
                if self.negative { -v } else { v }
            }
            None => parse_decimal(&self.text()),
        }
    }

    /// The input with thousands separators: `-1234.50` → `-1,234.50`.
    fn display(&self) -> String {
        let sign = if self.negative { "-" } else { "" };
        let unsigned = if self.digits.is_empty() {
            "0"
        } else {
            &self.digits
        };
        let number = match unsigned.split_once('.') {
            Some((int, frac)) => format!("{sign}{}.{frac}", group_thousands(int)),
            None => format!("{sign}{}", group_thousands(unsigned)),
        };
        format!("{number}{}", self.exponent_text())
    }
}

/// A value and an operation waiting on one of the stacks. On the precedence
/// stack, `op: None` marks where a parenthesis was opened.
#[derive(Clone, Debug)]
struct Pending {
    value: Number,
    op: Option<Op>,
}

/// A piece of the expression line. An operand keeps its value, so that a new radix can
/// write it again (`COpndCommand`).
#[derive(Clone, Debug)]
struct Token {
    text: String,
    value: Option<Number>,
}

impl Token {
    fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            value: None,
        }
    }
}

/// The expression line being built, as a list of tokens (`CHistoryCollector`).
/// Indices point into `tokens`; like in Windows, they are not adjusted when a
/// token is inserted before them.
#[derive(Clone, Debug, Default)]
struct HistoryLine {
    tokens: Vec<Token>,
    /// Where the last operand starts (`m_lastOpStartIndex`).
    last_operand_start: Option<usize>,
    /// Where the last binary operator starts (`m_lastBinOpStartIndex`).
    last_bin_op_start: Option<usize>,
    /// Operand starts saved for precedence and parentheses (`m_operandIndices`).
    operand_starts: Vec<Option<usize>>,
    /// The last operand is a parenthesized group (`m_bLastOpndBrace`).
    last_operand_brace: bool,
}

impl HistoryLine {
    fn push(&mut self, token: impl Into<String>) -> usize {
        self.tokens.push(Token::new(token));
        self.tokens.len() - 1
    }

    fn operand_added(&self) -> bool {
        self.last_operand_start.is_some()
    }

    fn add_operand(&mut self, text: String, value: Number) {
        self.tokens.push(Token {
            text,
            value: Some(value),
        });
        self.last_operand_start = Some(self.tokens.len() - 1);
        self.last_operand_brace = false;
        self.last_bin_op_start = None;
    }

    /// Writes every operand again (`UpdateHistoryExpression`, for a new radix).
    fn rewrite_operands(&mut self, write: impl Fn(&Number) -> String) {
        for token in &mut self.tokens {
            if let Some(value) = &token.value {
                token.text = write(value);
            }
        }
    }

    fn remove_last_operand(&mut self) {
        if let Some(start) = self.last_operand_start.take() {
            self.tokens.truncate(start);
        }
    }

    fn add_bin_op(&mut self, symbol: &str) {
        self.last_bin_op_start = Some(self.push(" "));
        self.push(symbol);
        self.push(" ");
        self.last_operand_start = None;
    }

    fn change_last_bin_op(&mut self, symbol: &str, precedence_inversion: bool) {
        if let Some(start) = self.last_bin_op_start {
            self.tokens.truncate(start);
        }
        if precedence_inversion {
            self.enclose_in_brackets();
        }
        self.add_bin_op(symbol);
    }

    fn push_operand_start(&mut self, start: Option<usize>) {
        if self.operand_starts.len() < MAX_DEPTH {
            self.operand_starts.push(start.or(self.last_operand_start));
        }
    }

    fn pop_operand_start(&mut self) {
        if let Some(start) = self.operand_starts.pop() {
            self.last_operand_start = start;
        }
    }

    fn add_open_brace(&mut self) {
        let start = self.push("(");
        self.push_operand_start(Some(start));
        self.last_bin_op_start = None;
    }

    fn add_close_brace(&mut self) {
        self.push(")");
        self.pop_operand_start();
        self.last_bin_op_start = None;
        self.last_operand_brace = true;
    }

    /// Puts brackets around the last operand group, when a precedence
    /// inversion turns `1 + 2 -` into `(1 + 2) ×`.
    fn enclose_in_brackets(&mut self) {
        let start = self.operand_starts.last().copied().flatten().unwrap_or(0);
        self.tokens
            .insert(start.min(self.tokens.len()), Token::new("("));
        self.push(")");
    }

    /// Wraps the last operand: `5` becomes `sqr(5)`, and a group `(1 + 2)` becomes `sqr(1 + 2)`.
    fn add_unary_op(&mut self, name: &str) {
        let text = if self.last_operand_brace {
            name.to_string()
        } else {
            format!("{name}(")
        };
        let at = self.last_operand_start.unwrap_or(self.tokens.len());
        self.tokens
            .insert(at.min(self.tokens.len()), Token::new(text));
        if !self.last_operand_brace {
            self.push(")");
        }
        self.last_operand_brace = false;
        self.last_bin_op_start = None;
    }

    fn render(&self) -> String {
        let text: String = self
            .tokens
            .iter()
            .map(|token| if token.text == "=" { " =" } else { &token.text })
            .collect();
        text.trim_end().to_string()
    }
}

#[derive(Clone, Debug)]
pub struct Calculator {
    /// Operator precedence and parentheses (`m_fPrecedence`, Scientific mode).
    precedence: bool,
    /// The Programmer mode's integer mode (`m_fIntegerMode`) with its radix and word size.
    integer: Option<IntegerMode>,
    /// How many digits can be typed and the display shows (`m_precision`): 16, or 32 in
    /// the Scientific mode.
    digits: i64,
    /// The trigonometric functions' unit (`m_angletype`).
    angle: Angle,
    /// `F-E`: numbers are written in scientific notation (`m_nFE`).
    notation: bool,
    input: Input,
    /// A number is being typed into `input` (`m_bRecord`).
    record: bool,
    /// `m_currentVal`, `m_lastVal` and `m_holdVal` (the right operand kept for repeated `=`).
    current: Number,
    last: Number,
    hold: Number,
    /// The pending operation (`m_nOpCode`) and the last one done (`m_nPrevOpCode`).
    op: Option<Op>,
    prev_op: Option<Op>,
    /// `current` is the result of an operation and `op` is pending (`m_bChangeOp`).
    change_op: bool,
    /// No `=` since the last operator (`m_bNoPrevEqu`).
    no_prev_equ: bool,
    /// The previous command and the current one (`m_nLastCom`, `m_nTempCom`).
    last_key: Key,
    this_key: Key,
    precedence_stack: Vec<Pending>,
    paren_stack: Vec<Pending>,
    error: Option<CalcError>,
    line: HistoryLine,
    /// The expression line as last shown; Windows refreshes it only at certain steps.
    shown: String,
    memory: Vec<Number>,
    history: Vec<HistoryEntry>,
}

impl Default for Calculator {
    fn default() -> Self {
        Self {
            precedence: false,
            integer: None,
            digits: MAX_DIGITS,
            angle: Angle::Degrees,
            notation: false,
            input: Input::default(),
            record: true,
            current: Number::zero(),
            last: Number::zero(),
            hold: Number::zero(),
            op: None,
            prev_op: None,
            change_op: false,
            no_prev_equ: true,
            last_key: Key::None,
            this_key: Key::None,
            precedence_stack: Vec::new(),
            paren_stack: Vec::new(),
            error: None,
            line: HistoryLine::default(),
            shown: String::new(),
            memory: Vec::new(),
            history: Vec::new(),
        }
    }
}

impl Calculator {
    /// The Scientific mode's engine: operator precedence, parentheses, 32 digits and the
    /// scientific functions (`SetScientificMode`).
    pub fn with_precedence() -> Self {
        Self {
            precedence: true,
            digits: SCIENTIFIC_DIGITS,
            ..Self::default()
        }
    }

    /// Whether this is the Scientific mode's engine.
    fn scientific(&self) -> bool {
        self.precedence && self.integer.is_none()
    }

    /// The trigonometric functions' unit.
    pub fn angle(&self) -> Angle {
        self.angle
    }

    /// Whether `F-E` is on.
    pub fn notation(&self) -> bool {
        self.notation
    }

    /// The Programmer mode's engine: precedence, parentheses and integer mode, starting
    /// in decimal with 64-bit words, and no history (Windows' `SetProgrammerMode`).
    pub fn programmer() -> Self {
        Self {
            precedence: true,
            integer: Some(IntegerMode::default()),
            ..Self::default()
        }
    }

    // --------------------------------------------------------------- queries

    /// The big number on the display (or the error message). In the Programmer mode
    /// it is in the current radix, in digit groups, and a binary number starts with a
    /// whole group of 4 digits.
    pub fn display(&self) -> String {
        if let Some(error) = self.error {
            return error.message().to_string();
        }
        if let Some(mode) = &self.integer {
            let text = if self.record {
                integer::group(&self.input.text(), mode.radix)
            } else {
                integer::group(&mode.format(&self.current, mode.radix), mode.radix)
            };
            return if mode.radix == Radix::Binary {
                integer::pad_binary(&text)
            } else {
                text
            };
        }
        if self.record {
            self.input.display()
        } else {
            self.format_value(&self.current)
        }
    }

    /// The Programmer mode's radix.
    pub fn radix(&self) -> Option<Radix> {
        self.integer.map(|mode| mode.radix)
    }

    /// The Programmer mode's word size.
    pub fn word_size(&self) -> Option<WordSize> {
        self.integer.map(|mode| mode.word)
    }

    /// The value in every radix for the Programmer mode's radix list, in the order of
    /// `Radix::ALL`, grouped like the display. Empty in the error state, as in Windows.
    pub fn radix_values(&self) -> Option<[String; 4]> {
        let mode = self.integer.as_ref()?;
        if self.error.is_some() {
            return Some(Default::default());
        }
        let value = self.shown_value();
        Some(Radix::ALL.map(|radix| {
            let text = integer::group(&mode.format(&value, radix), radix);
            if radix == Radix::Binary {
                integer::pad_binary(&text)
            } else {
                text
            }
        }))
    }

    /// The bits of the value shown, for the bit toggling keypad (bit 0 is the lowest).
    pub fn bits(&self) -> u64 {
        let Some(mode) = &self.integer else {
            return 0;
        };
        let value = mode.truncate(&self.shown_value());
        value.to_integer().to_u64().unwrap_or_default()
    }

    /// The number typed or the result shown (`GetCurrentResultForRadix`).
    fn shown_value(&self) -> Number {
        if self.record {
            self.input.value(self.integer.as_ref())
        } else {
            self.current.clone()
        }
    }

    /// Whether nothing has been typed and the display shows 0, when the Programmer
    /// mode's keypad shows `C` instead of `CE` (`CCalcEngine::IsInputEmpty`).
    pub fn is_input_empty(&self) -> bool {
        self.input.is_empty() && (self.record || self.number_text() == "0")
    }

    /// A value as the display and the memory list show it: with this mode's digits and
    /// notation, in the Programmer mode in its radix (without the binary padding).
    pub fn format_value(&self, v: &Number) -> String {
        match &self.integer {
            Some(mode) => integer::group(&mode.format(v, mode.radix), mode.radix),
            None => format_with(v, true, self.digits, self.notation),
        }
    }

    /// The expression line above the display, e.g. `5 + sqr(3)`.
    pub fn expression(&self) -> String {
        self.shown.clone()
    }

    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }

    /// The error the display shows, if any.
    pub fn error(&self) -> Option<CalcError> {
        self.error
    }

    /// How many parentheses are open (the `(` counter of the Scientific mode).
    pub fn open_parentheses(&self) -> usize {
        self.paren_stack.len()
    }

    pub fn memory(&self) -> &[Number] {
        &self.memory
    }

    /// Calculation history, newest first.
    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    /// Whether a button is enabled (most buttons are disabled in the error state).
    pub fn is_enabled(&self, command: Command) -> bool {
        if let Some(mode) = &self.integer {
            // The Programmer mode: only the radix's digits, no decimal point and no
            // Standard functions; `=`, the radix list and the bits stay enabled in the
            // error state, as in `CalculatorProgrammerRadixOperators.xaml`.
            match command {
                Command::Digit(digit) => return u32::from(digit) < mode.radix.base(),
                Command::Decimal
                | Command::Percent
                | Command::Pi
                | Command::Euler
                | Command::Random
                | Command::Exponent
                | Command::ToggleNotation
                | Command::Angle(_) => return false,
                Command::Operator(op) if op.is_scientific() => return false,
                Command::Function(func) if !func.is_integer() && func != Func::Negate => {
                    return false;
                }
                Command::Equals | Command::Radix(_) => return true,
                Command::ToggleBit(bit) => return u32::from(bit) < mode.word.bits(),
                _ => {}
            }
        } else {
            // The Programmer mode's keys don't exist elsewhere, nor the Scientific mode's
            // in the Standard mode (but for `exp`, which paste uses).
            let scientific = self.scientific();
            let other_mode = match command {
                Command::ToggleBit(_) | Command::Radix(_) | Command::WordSize(_) => true,
                Command::Digit(digit) => digit > 9,
                Command::Operator(op) => {
                    op.is_integer() || (!scientific && (op.is_scientific() || op == Op::Mod))
                }
                Command::Function(func) => {
                    func.is_integer() || (!scientific && func.is_scientific())
                }
                Command::Pi
                | Command::Euler
                | Command::Random
                | Command::ToggleNotation
                | Command::Angle(_) => !scientific,
                _ => false,
            };
            if other_mode {
                return false;
            }
        }
        match command {
            // In the error state the number pad's `ErrorLayout` disables the decimal point,
            // and `=` stays enabled (it clears the error). The Scientific mode's unit and F-E
            // buttons stay enabled too: Windows sends their control to visual states that
            // don't exist (`ErrorFlyout`).
            Command::Digit(_)
            | Command::Clear
            | Command::ClearEntry
            | Command::Backspace
            | Command::Equals
            | Command::ToggleNotation
            | Command::Angle(_) => true,
            Command::MemoryClear | Command::MemoryRecall => {
                self.error.is_none() && !self.memory.is_empty()
            }
            Command::OpenParen | Command::CloseParen => self.precedence && self.error.is_none(),
            _ => self.error.is_none(),
        }
    }

    /// Text to copy to the clipboard (without thousands separators, and in the
    /// Programmer mode without the spaces between digit groups).
    pub fn copy_text(&self) -> Option<String> {
        if self.error.is_some() {
            None
        } else {
            Some(self.display().replace([',', ' '], ""))
        }
    }

    // -------------------------------------------------------------- commands

    pub fn execute(&mut self, command: Command) {
        if !self.is_enabled(command) {
            return;
        }
        if self.error.is_some() {
            // Windows' view model clears the error first; only digits and the
            // decimal point (and in the Programmer mode a bit or a new radix) then
            // go on (`IsRecoverableCommand`, `SwitchProgrammerModeBase`). The unit and
            // F-E buttons change what they show in Windows but not the engine; here
            // they apply, so the two agree.
            self.process(Command::Clear);
            if !matches!(
                command,
                Command::Digit(_)
                    | Command::Decimal
                    | Command::ToggleBit(_)
                    | Command::Radix(_)
                    | Command::ToggleNotation
                    | Command::Angle(_)
            ) {
                return;
            }
        } else if self.notation && matches!(command, Command::Clear | Command::ClearEntry) {
            // The view model turns F-E off for C and CE (`IsFToEChecked = false`), and
            // unchecking its button sends `IDC_FE` before the clear.
            self.process(Command::ToggleNotation);
        }
        match command {
            Command::MemoryStore => self.memory_store(),
            Command::MemoryAdd => self.memory_add(0, false),
            Command::MemorySubtract => self.memory_add(0, true),
            Command::MemoryRecall => self.memory_recall_item(0),
            Command::MemoryClear => {
                self.process(command);
                self.memory.clear();
            }
            _ => self.process(command),
        }
    }

    /// One command through the engine (`ProcessCommandWorker`).
    fn process(&mut self, command: Command) {
        // When `begin` finds the typed number too large (`99.e+9999`), Windows goes on with
        // the command all the same: `99 exp 9999 +` shows `Overflow` and `9.9e+10000 +`.
        self.begin(command);
        match command {
            Command::Digit(digit) => self.digit(digit),
            Command::Decimal => self.point(),
            Command::Operator(op) => self.binary(op),
            Command::Function(Func::Negate) => self.negate(),
            Command::Function(func) => self.unary(func),
            Command::Percent => self.percent(),
            Command::Equals => self.equals(),
            Command::Clear => self.clear(),
            Command::ClearEntry => self.clear_entry(),
            Command::Backspace => {
                if self.record {
                    self.input.backspace();
                }
            }
            Command::OpenParen => self.open_paren(),
            Command::CloseParen => self.close_paren(),
            Command::ToggleBit(bit) => self.toggle_bit(bit),
            Command::Radix(radix) => self.set_radix(radix),
            Command::WordSize(word) => self.set_word_size(word),
            Command::Pi => self.constant(math::pi()),
            Command::Euler => self.constant(math::euler()),
            Command::Random => self.constant(math::random()),
            // `exp` works only while a number is typed; otherwise it's ignored (a setting,
            // so there is no command to forget).
            Command::Exponent => {
                if self.record && self.integer.is_none() {
                    self.input.try_begin_exponent();
                }
            }
            Command::ToggleNotation => self.notation = !self.notation,
            Command::Angle(angle) => self.angle = angle,
            // Only the steps in `begin` (finishing the typed number) apply here.
            Command::MemoryClear
            | Command::MemoryRecall
            | Command::MemoryAdd
            | Command::MemorySubtract
            | Command::MemoryStore => {}
        }
    }

    /// The steps every command starts with.
    fn begin(&mut self, command: Command) {
        if let Some(key) = Key::of(command) {
            self.last_key = self.this_key;
            self.this_key = key;
        }
        // Any action hides the expression shown after `=`.
        if !self.no_prev_equ {
            self.shown.clear();
        }
        let ends_typing = matches!(
            command,
            Command::Operator(_)
                | Command::Percent
                | Command::Equals
                | Command::OpenParen
                | Command::CloseParen
                | Command::MemoryClear
                | Command::MemoryRecall
                | Command::MemoryAdd
                | Command::MemorySubtract
                | Command::MemoryStore
                | Command::ToggleBit(_)
                | Command::Radix(_)
                | Command::WordSize(_)
                | Command::Pi
                | Command::Euler
                | Command::Random
                | Command::ToggleNotation
                | Command::Angle(_)
        ) || matches!(command, Command::Function(func) if func != Func::Negate)
            // `±` outside decimal works on the finished number.
            || (command == Command::Function(Func::Negate)
                && self.radix().is_some_and(|radix| radix != Radix::Decimal));
        if self.record {
            if ends_typing {
                self.record = false;
                self.current = self.input.value(self.integer.as_ref());
                self.display_num();
                // A typed exponent can make the number too large for the display, which
                // Windows' `DisplayNum` reports as an overflow.
                if self.integer.is_none()
                    && let Err(error) = normalize(self.current.clone())
                {
                    self.display_error(error);
                }
            }
        } else if matches!(command, Command::Digit(_) | Command::Decimal) {
            self.record = true;
            self.input.clear();
            // `(8)2` continues the equation instead of starting a new one.
            if self.last_key != Key::CloseParen {
                self.check_and_add_last_bin_op_to_history(true);
            }
        }
    }

    /// An invalid command is ignored as if it never happened (`HandleErrorCommand`).
    fn forget_command(&mut self) {
        self.this_key = self.last_key;
    }

    /// In the Programmer mode, the steps of Windows' `DisplayNum` that change the value:
    /// a result becomes an unsigned integer of the word size, so `5 ÷ 2 × 2` is 4.
    fn display_num(&mut self) {
        if let Some(mode) = &self.integer
            && !self.record
        {
            self.current = mode.truncate(&self.current);
        }
    }

    fn digit(&mut self, digit: u8) {
        let max = self.digits as usize;
        if !self.input.try_add_digit(digit, self.integer.as_ref(), max) {
            return self.forget_command();
        }
        if self.last_key == Key::CloseParen {
            self.implicit_multiplication();
        }
    }

    fn point(&mut self) {
        if self.last_key == Key::CloseParen {
            self.implicit_multiplication();
        }
        if !(self.record && self.input.try_add_point()) {
            self.forget_command();
        }
    }

    /// A number right after `)` multiplies: `(8)2` is `(8) × 2`.
    fn implicit_multiplication(&mut self) {
        self.op = Some(Op::Multiply);
        self.last = self.current.clone();
        self.hold = Number::zero();
        self.no_prev_equ = true;
        if !self.line.operand_added() {
            self.line.add_open_brace();
            self.add_operand();
            self.line.add_close_brace();
        }
        self.add_bin_op(Op::Multiply, true);
        self.change_op = true;
        self.prev_op = None;
        self.precedence_stack.clear();
    }

    fn binary(&mut self, op: Op) {
        // Another operator right after one replaces it.
        if let Key::Operator(replaced) = self.last_key {
            self.op = Some(op);
            // `1 + 2 -` changed to `×`: 1 + 2 was already computed, so it gets brackets.
            let mut inversion = false;
            if self.precedence && self.prev_op.is_some() {
                let previous = precedence(self.prev_op);
                if replaced.precedence() <= previous && op.precedence() > previous {
                    inversion = true;
                    self.prev_op = None;
                }
            }
            self.line
                .change_last_bin_op(op.symbol(self.integer.is_some()), inversion);
            self.refresh();
            return;
        }

        if !self.line.operand_added() {
            self.add_operand();
        }
        if self.change_op {
            loop {
                let new = op.precedence();
                let pending = precedence(self.op);
                if new > pending && self.precedence {
                    // The new operator binds tighter: keep the pending one for later.
                    if self.precedence_stack.len() < MAX_DEPTH {
                        self.precedence_stack.push(Pending {
                            value: self.last.clone(),
                            op: self.op,
                        });
                        self.line.push_operand_start(None);
                    } else {
                        self.forget_command();
                    }
                    break;
                }
                self.current = self.operate(self.op);
                self.prev_op = self.op;
                if self.error.is_none() {
                    self.display_num();
                    if !self.precedence {
                        // Standard mode logs every step and continues from its result.
                        self.complete_equation();
                        self.add_operand();
                    }
                }
                match self.precedence_stack.last() {
                    Some(Pending { op: Some(_), .. }) => {
                        let Pending { value, op: popped } = self.precedence_stack.pop().unwrap();
                        self.op = popped;
                        self.last = value;
                        if pending <= precedence(popped) {
                            self.line.enclose_in_brackets();
                        }
                        self.line.pop_operand_start();
                    }
                    _ => break,
                }
            }
        }
        self.last = self.current.clone();
        self.op = Some(op);
        self.add_bin_op(op, true);
        self.no_prev_equ = true;
        self.change_op = true;
    }

    fn unary(&mut self, func: Func) {
        // `5 + √` works on the 5 (so `5 + 1/x` isn't a division by zero).
        if matches!(self.last_key, Key::Operator(_)) {
            self.current = self.last.clone();
        }
        if !self.line.operand_added() {
            self.add_operand();
        }
        self.line.add_unary_op(&func.name(self.angle));
        self.refresh();
        if func == Func::Degrees && !self.no_prev_equ {
            // `→deg` is Windows' old inverse of `dms`: `SciCalcFunctions` sends `IDC_INV`
            // through the engine, which hides the expression shown after `=` again.
            self.shown.clear();
        }
        if matches!(func, Func::Trig(_)) && math::too_big_for_trig(&self.current) {
            self.current = Number::zero();
            return self.display_error(CalcError::InvalidInput);
        }
        let result = match (&mut self.integer, func) {
            (Some(mode), Func::Not) => Ok(mode.not(&self.current)),
            (Some(mode), Func::RotateLeft) => Ok(mode.rotate(&self.current, true, false)),
            (Some(mode), Func::RotateRight) => Ok(mode.rotate(&self.current, false, false)),
            (Some(mode), Func::RotateLeftCarry) => Ok(mode.rotate(&self.current, true, true)),
            (Some(mode), Func::RotateRightCarry) => Ok(mode.rotate(&self.current, false, true)),
            _ => func.apply(&self.current, self.angle),
        };
        match result {
            Ok(result) => {
                self.current = result;
                self.display_num();
            }
            Err(error) => self.display_error(error),
        }
    }

    fn negate(&mut self) {
        // While typing, ± just flips the sign of the number being typed.
        if self.record {
            if !self.input.toggle_sign(self.integer.as_ref()) {
                self.forget_command();
            }
            return;
        }
        if matches!(self.last_key, Key::Operator(_)) {
            self.current = self.last.clone();
        }
        if !self.line.operand_added() {
            self.add_operand();
        }
        self.current = -self.current.clone();
        self.display_num();
        self.line.add_unary_op(&Func::Negate.name(self.angle));
        self.refresh();
    }

    /// π, e or a random number, which Windows treats like typing a number: it replaces
    /// the last operand (`IDC_PI`, `IDC_EULER`, `IDC_RAND`). A number next to it doesn't
    /// multiply, so `2π` is π.
    fn constant(&mut self, value: Number) {
        self.check_and_add_last_bin_op_to_history(true);
        self.current = value;
        self.display_num();
    }

    /// `%` as in Windows: after `×` or `÷` the value divided by 100, otherwise a
    /// percentage of the left operand (0 when there is none). The expression
    /// line shows the result, not a `%` sign.
    fn percent(&mut self) {
        if matches!(self.last_key, Key::Operator(_)) {
            self.current = self.last.clone();
        }
        let hundred = Number::from_integer(100.into());
        let result = if matches!(self.op, Some(Op::Multiply | Op::Divide)) {
            &self.current / hundred
        } else {
            &self.current * (&self.last / hundred)
        };
        match normalize(result) {
            Ok(result) => self.current = result,
            Err(error) => return self.display_error(error),
        }
        self.check_and_add_last_bin_op_to_history(true);
        self.add_operand();
        self.refresh();
    }

    fn equals(&mut self) {
        // `=` closes all open parentheses first.
        while !self.paren_stack.is_empty() {
            if self.error.is_some() {
                break;
            }
            self.this_key = self.last_key;
            self.process(Command::CloseParen);
            self.last_key = self.this_key;
            self.this_key = Key::Equals;
        }
        if !self.no_prev_equ {
            self.last = self.current.clone();
        }
        // `5 + =` repeats the left operand: 5 + 5.
        if matches!(self.last_key, Key::Operator(_)) {
            self.current = self.last.clone();
        }
        if !self.line.operand_added() {
            self.add_operand();
        }
        self.resolve_pending();
        while self.precedence {
            let Some(Pending { value, op }) = self.precedence_stack.pop() else {
                break;
            };
            self.op = op;
            self.last = value;
            if precedence(self.prev_op) <= precedence(op) {
                self.line.enclose_in_brackets();
            }
            self.line.pop_operand_start();
            self.no_prev_equ = true;
            self.resolve_pending();
        }
        if self.error.is_none() {
            self.complete_equation();
            self.last = self.current.clone();
            self.prev_op = None;
            self.precedence_stack.clear();
        }
        self.change_op = false;
    }

    /// Does the pending operation; repeated `=` reuses the right operand
    /// (`ResolveHighestPrecedenceOperation`).
    fn resolve_pending(&mut self) {
        let Some(op) = self.op else {
            if self.error.is_none() {
                self.display_num();
            }
            return;
        };
        if self.no_prev_equ {
            self.hold = self.current.clone();
        } else {
            self.current = self.hold.clone();
            self.display_num();
            self.line.add_bin_op(op.symbol(self.integer.is_some()));
            self.add_operand();
        }
        self.current = self.operate(Some(op));
        self.prev_op = Some(op);
        self.last = self.current.clone();
        if self.error.is_none() {
            self.display_num();
        }
        self.no_prev_equ = false;
    }

    fn open_paren(&mut self) {
        let stack_full = matches!(
            self.precedence_stack.last(),
            Some(Pending { op: Some(_), .. })
        ) && self.precedence_stack.len() >= MAX_DEPTH;
        if self.paren_stack.len() >= MAX_DEPTH || stack_full {
            return self.forget_command();
        }
        // A number or `)` before `(` multiplies: `2(3)` is `2 × (3)`.
        if matches!(
            self.last_key,
            Key::Digit | Key::Unary | Key::Point | Key::CloseParen
        ) {
            self.process(Command::Operator(Op::Multiply));
        }
        self.check_and_add_last_bin_op_to_history(true);
        self.line.add_open_brace();
        self.refresh();
        self.paren_stack.push(Pending {
            value: self.last.clone(),
            op: if self.change_op { self.op } else { None },
        });
        if self.precedence_stack.len() < MAX_DEPTH {
            self.precedence_stack.push(Pending {
                value: Number::zero(),
                op: None,
            });
        }
        self.last = Number::zero();
        // `1 + (` starts the group at 0, but `1 + 3 (` starts it at 3.
        if matches!(self.last_key, Key::Operator(_)) {
            self.current = Number::zero();
        }
        self.this_key = Key::None;
        self.op = None;
        self.change_op = false;
        if self.error.is_none() {
            self.display_num();
        }
    }

    fn close_paren(&mut self) {
        if self.paren_stack.is_empty() {
            return self.forget_command();
        }
        if matches!(self.last_key, Key::Operator(_)) {
            self.current = self.last.clone();
        }
        if !self.line.operand_added() {
            self.add_operand();
        }
        self.current = self.operate(self.op);
        self.prev_op = self.op;
        // Resolve everything pending inside the parentheses.
        while let Some(Pending {
            value,
            op: Some(op),
        }) = self.precedence_stack.pop()
        {
            if precedence(self.prev_op) <= op.precedence() {
                self.line.enclose_in_brackets();
            }
            self.line.pop_operand_start();
            self.op = Some(op);
            self.last = value;
            self.current = self.operate(Some(op));
            self.prev_op = Some(op);
        }
        self.line.add_close_brace();
        self.refresh();
        let Pending { value, op } = self.paren_stack.pop().expect("checked above");
        self.last = value;
        self.op = op;
        self.change_op = op.is_some();
        if self.error.is_none() {
            self.display_num();
        }
    }

    /// Resets the calculation; memory and history are kept (`IDC_CLEAR`).
    fn clear(&mut self) {
        if !self.change_op {
            self.check_and_add_last_bin_op_to_history(false);
        }
        if let Some(mode) = &mut self.integer {
            mode.clear();
        }
        self.last = Number::zero();
        self.change_op = false;
        self.paren_stack.clear();
        self.precedence_stack.clear();
        self.last_key = Key::None;
        self.this_key = Key::None;
        self.op = None;
        self.prev_op = None;
        self.no_prev_equ = true;
        self.shown.clear();
        self.line = HistoryLine::default();
        self.clear_entry();
    }

    /// Clears the number being entered (`ClearTemporaryValues`).
    fn clear_entry(&mut self) {
        self.input.clear();
        self.record = true;
        self.check_and_add_last_bin_op_to_history(true);
        self.error = None;
    }

    /// `current op` applied to `last` (`DoOperation`); on an error `current` stays.
    fn operate(&mut self, op: Option<Op>) -> Number {
        let Some(op) = op else {
            return self.current.clone();
        };
        let result = match &self.integer {
            Some(mode) => mode.apply(&self.last, op, &self.current),
            None => apply(&self.last, op, &self.current),
        };
        match result {
            Ok(result) => result,
            Err(error) => {
                self.display_error(error);
                self.current.clone()
            }
        }
    }

    /// Shows an error. The expression line stays as it is.
    fn display_error(&mut self, error: CalcError) {
        self.error = Some(error);
    }

    /// The current value as it appears in the expression line: in the Programmer mode
    /// in its radix, signed in decimal.
    fn number_text(&self) -> String {
        match &self.integer {
            Some(mode) => mode.format(&self.current, mode.radix),
            None => format_with(&self.current, false, self.digits, self.notation),
        }
    }

    // -------------------------------------------------------- expression line

    fn refresh(&mut self) {
        self.shown = self.line.render();
    }

    /// Adds the current value to the expression line.
    fn add_operand(&mut self) {
        let text = self.number_text();
        self.line.add_operand(text, self.current.clone());
    }

    fn add_bin_op(&mut self, op: Op, refresh: bool) {
        self.line.add_bin_op(op.symbol(self.integer.is_some()));
        if refresh {
            self.refresh();
        }
    }

    /// Ends the equation with `=` and logs it.
    fn complete_equation(&mut self) {
        self.line.push("=");
        self.refresh();
        self.complete_history_line();
    }

    /// Logs the expression so far with the current result and starts a new one. The
    /// Programmer mode has no history.
    fn complete_history_line(&mut self) {
        if self.integer.is_none() {
            let expression = self.line.render();
            self.history.insert(
                0,
                HistoryEntry {
                    expression,
                    result: self.current.clone(),
                    text: self.format_value(&self.current),
                },
            );
            self.history.truncate(HISTORY_LIMIT);
        }
        self.line = HistoryLine::default();
    }

    /// Makes the expression line end with a binary operator again when a new
    /// operand starts (`CheckAndAddLastBinOpToHistory`). After `1 + √(4)`, typing
    /// replaces `√(4)`; after a finished `√(4)`, typing logs it and starts anew.
    fn check_and_add_last_bin_op_to_history(&mut self, add_to_history: bool) {
        if self.change_op {
            if self.line.operand_added() {
                self.line.remove_last_operand();
                self.refresh();
            }
        } else if self.line.operand_added() && self.error.is_none() {
            let finished = matches!(self.last_key, Key::Unary | Key::Sign | Key::CloseParen)
                && self.paren_stack.is_empty();
            if finished {
                if add_to_history {
                    self.complete_history_line();
                }
            } else {
                self.line.remove_last_operand();
                self.refresh();
            }
        }
    }

    // ---------------------------------------------------- memory and history

    /// A value for the memory: in the Programmer mode an integer of the word size, as
    /// the memory follows the integer mode (`TruncateNumForIntMath`).
    fn for_memory(&self, v: Number) -> Number {
        match &self.integer {
            Some(mode) => mode.truncate(&v),
            None => v,
        }
    }

    fn memory_store(&mut self) {
        self.process(Command::MemoryStore);
        let value = self.for_memory(self.current.clone());
        self.memory.insert(0, value);
        self.memory.truncate(MEMORY_LIMIT);
    }

    /// Puts memory slot `index` on the display (MR, or a click in the memory list).
    pub fn memory_recall_item(&mut self, index: usize) {
        let Some(value) = self.memory.get(index).cloned() else {
            return;
        };
        if self.error.is_some() {
            return;
        }
        self.begin(Command::MemoryRecall);
        // Windows assigns the recalled value first, but logs a finished function with
        // `m_numberString`, the text of the number that was on the display (`IDC_RECALL`),
        // so the history keeps the function's result, not the recalled value.
        self.check_and_add_last_bin_op_to_history(true);
        self.current = value;
        self.display_num();
    }

    /// M+ (or M− when `subtract`) for memory slot `index`. With an empty memory,
    /// M+ stores the display and M− stores its negative.
    pub fn memory_add(&mut self, index: usize, subtract: bool) {
        if self.error.is_some() {
            return;
        }
        if self.memory.is_empty() {
            self.memory_store();
            if subtract {
                self.memory[0] = self.for_memory(-&self.memory[0]);
            }
            return;
        }
        let command = if subtract {
            Command::MemorySubtract
        } else {
            Command::MemoryAdd
        };
        self.process(command);
        if let Some(cell) = self.memory.get(index) {
            let sum = if subtract {
                cell - &self.current
            } else {
                cell + &self.current
            };
            if let Ok(sum) = normalize(sum) {
                self.memory[index] = self.for_memory(sum);
            }
        }
    }

    /// The memory, to share it with another mode's engine: Windows has one memory for
    /// the Standard and Programmer modes.
    pub fn take_memory(&mut self) -> Vec<Number> {
        std::mem::take(&mut self.memory)
    }

    pub fn set_memory(&mut self, memory: Vec<Number>) {
        self.memory = memory;
    }

    /// Starts over, as when Windows switches to this mode: `C`, and the Programmer mode
    /// goes back to decimal (`SetStandardMode`, `SetScientificMode`, `SetProgrammerMode`).
    /// F-E is off: the view model turns it off when the Scientific mode is left. Memory,
    /// history, the word size and the angle unit stay.
    pub fn reset(&mut self) {
        self.process(Command::Clear);
        self.notation = false;
        if self.integer.is_some() {
            self.process(Command::Radix(Radix::Decimal));
        }
    }

    // ------------------------------------------------------------ programmer

    /// Flips a bit of the value (the bit toggling keypad, `IDC_BINPOS0` …). Right after
    /// an operator it works on the left operand, like a function.
    fn toggle_bit(&mut self, bit: u8) {
        if matches!(self.last_key, Key::Operator(_)) {
            self.current = self.last.clone();
        }
        self.check_and_add_last_bin_op_to_history(true);
        let Some(mode) = &self.integer else {
            return;
        };
        if let Some(value) = mode.toggle_bit(&self.current, bit.into()) {
            self.current = value;
            self.display_num();
        }
    }

    /// Switches the radix. The value stays; the expression line is written again in the
    /// new radix (`UpdateHistoryExpression`), from the operands' unsigned values, so a
    /// negative decimal operand comes back as a large positive number, as in Windows.
    fn set_radix(&mut self, radix: Radix) {
        let Some(mode) = &mut self.integer else {
            return;
        };
        mode.radix = radix;
        self.display_num();
        if !self.line.tokens.is_empty() {
            self.line
                .rewrite_operands(|v| integer::to_radix(&v.to_integer(), radix));
            self.refresh();
        }
    }

    /// Switches the word size. A value whose highest bit is set counts as negative, so
    /// a larger word extends its sign and a smaller one keeps only the low bits
    /// (`SetRadixTypeAndNumWidth`).
    fn set_word_size(&mut self, word: WordSize) {
        let Some(mode) = &mut self.integer else {
            return;
        };
        self.current = mode.signed(&self.current);
        mode.word = word;
        self.display_num();
    }

    pub fn memory_clear_item(&mut self, index: usize) {
        if index < self.memory.len() {
            self.memory.remove(index);
        }
    }

    /// Shows a history entry's result and expression, as after `=`.
    pub fn recall_history(&mut self, index: usize) {
        let Some(entry) = self.history.get(index).cloned() else {
            return;
        };
        self.process(Command::Clear);
        self.current = entry.result.clone();
        self.last = entry.result;
        self.record = false;
        self.shown = entry.expression;
        // The next key clears the expression line, as after `=`.
        self.no_prev_equ = false;
        self.this_key = Key::Equals;
    }

    pub fn delete_history_item(&mut self, index: usize) {
        if index < self.history.len() {
            self.history.remove(index);
        }
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// Pastes text the way Windows does: the text is checked with the rules of
    /// `CopyPasteManager`, then typed in key by key after a `CE`, so `12 + 5 =`
    /// calculates 17. Invalid text shows `Invalid input`. Returns whether the
    /// text was accepted.
    pub fn paste(&mut self, text: &str) -> bool {
        let valid = match &self.integer {
            Some(mode) => programmer_paste_is_valid(text, mode),
            None => paste_is_valid(text, self.precedence),
        };
        if !valid {
            self.display_error(CalcError::InvalidInput);
            return false;
        }
        // `OnPaste` sends CE straight to the engine, so F-E stays on (only the view
        // model's C and CE buttons turn it off), and in the error state the engine takes
        // CE as C.
        self.process(if self.error.is_some() {
            Command::Clear
        } else {
            Command::ClearEntry
        });

        // A `-` at the start or after an operator is a sign. Windows presses ±
        // after the first digit that isn't 0, and remembers a sign before `(`
        // until the matching `)`. In the Programmer mode the letters are hex
        // digits and `%` is the remainder; other characters, like the `x` of `0x1F`
        // or a `u` suffix, are skipped. Elsewhere `e` is `exp`, and a sign after it
        // goes to the exponent; the Scientific mode has `^` and `%` (mod) too.
        let integer = self.integer.is_some();
        let scientific = self.scientific();
        let mut first = true;
        let mut previous_operator = false;
        let mut negate = false;
        let mut negate_stack = Vec::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            let (command, mut can_negate) = match c {
                // A 0 never takes the sign (`MapCharacterToButtonId`), or each 0 of `-0.05`
                // would flip it.
                '0' => (Command::Digit(0), false),
                '1'..='9' => (Command::Digit(c as u8 - b'0'), true),
                'a'..='f' | 'A'..='F' if integer => {
                    let digit = c.to_digit(16).unwrap_or_default() as u8;
                    (Command::Digit(digit), false)
                }
                'e' | 'E' => (Command::Exponent, false),
                '%' if integer || scientific => (Command::Operator(Op::Mod), false),
                '^' if scientific => (Command::Operator(Op::Power), false),
                '.' => (Command::Decimal, false),
                '+' => (Command::Operator(Op::Add), false),
                '-' => (Command::Operator(Op::Subtract), false),
                '*' => (Command::Operator(Op::Multiply), false),
                '/' => (Command::Operator(Op::Divide), false),
                '=' => (Command::Equals, false),
                '(' => (Command::OpenParen, false),
                ')' => (Command::CloseParen, false),
                _ => continue,
            };
            let mut send = true;
            if first || previous_operator {
                first = false;
                previous_operator = false;
                match command {
                    Command::Operator(Op::Subtract) => {
                        negate = true;
                        send = false;
                    }
                    Command::Operator(Op::Add) => send = false,
                    _ => {}
                }
            }
            match command {
                Command::OpenParen => {
                    negate_stack.push(negate);
                    negate = false;
                }
                Command::CloseParen => match negate_stack.pop() {
                    Some(saved) => {
                        negate = saved;
                        can_negate = true;
                    }
                    None => send = false,
                },
                // Not after `%` (`OnPaste` counts only these four).
                Command::Operator(Op::Add | Op::Subtract | Op::Multiply | Op::Divide) => {
                    previous_operator = true
                }
                _ => {}
            }
            if send {
                self.paste_key(command);
                if negate {
                    if can_negate {
                        self.paste_key(Command::Function(Func::Negate));
                    }
                    if !matches!(command, Command::Digit(0) | Command::Decimal) {
                        negate = false;
                    }
                }
            }
            // The exponent's sign: ± flips it once `exp` was pressed.
            if command == Command::Exponent {
                match chars.peek() {
                    Some('-') => {
                        self.paste_key(Command::Function(Func::Negate));
                        chars.next();
                    }
                    Some('+') => {
                        chars.next();
                    }
                    _ => {}
                }
            }
        }
        true
    }

    /// One pasted key. `OnPaste` sends it straight to the engine, without the view
    /// model's error recovery, and in the error state the engine ignores every key but C
    /// and CE (`HandleErrorCommand`): `1/0*2=` stays `Cannot divide by zero`.
    fn paste_key(&mut self, command: Command) {
        if self.error.is_none() {
            self.execute(command);
        }
    }
}

// ----------------------------------------------------------------- paste

/// The characters `CopyPasteManager::ExtractOperands` looks at, per mode. The
/// converters use the basic set.
pub(crate) const BASIC_PASTE_CHARACTERS: &str = "0123456789+-.e";
const STANDARD_PASTE_CHARACTERS: &str = "0123456789+-.e*/";
const SCIENTIFIC_PASTE_CHARACTERS: &str = "0123456789+-.e*/()^%";

/// The first steps of `CopyPasteManager::ValidatePasteExpression`, shared with the
/// converters: `None` if the text is too long, otherwise the text without the
/// characters Windows ignores (spaces, grouping commas, quotes, currency signs)
/// and without a trailing `=`.
pub(crate) fn clean_paste(text: &str) -> Option<String> {
    const UNWANTED: [char; 19] = [
        ' ', ',', '"', '¥', '¤', '₵', '$', '₡', '₩', '₪', '₦', '₹', '£', '€', '\u{202a}',
        '\u{202b}', '\u{202c}', '\u{202d}', '\u{a0}',
    ];
    if text.chars().count() > MAX_PASTE_LENGTH {
        return None;
    }
    let mut expression: String = text.chars().filter(|c| !UNWANTED.contains(c)).collect();
    if expression.ends_with('=') {
        expression.pop();
    }
    Some(expression)
}

/// Checks pasted text like Windows' `CopyPasteManager::ValidatePasteExpression`.
fn paste_is_valid(text: &str, scientific: bool) -> bool {
    let Some(expression) = clean_paste(text) else {
        return false;
    };
    if scientific && !expression.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    let valid = if scientific {
        SCIENTIFIC_PASTE_CHARACTERS
    } else {
        STANDARD_PASTE_CHARACTERS
    };
    let operands = extract_operands(&expression, valid);
    let max_length = if scientific { 32 } else { MAX_DIGITS as usize };
    !operands.is_empty()
        && operands.iter().all(|operand| {
            number_matches(operand, scientific, true) && operand_length(operand) <= max_length
        })
}

/// Checks pasted text like `CopyPasteManager` does in the Programmer mode: every operand
/// must be a number of the radix that fits into the word size.
fn programmer_paste_is_valid(text: &str, mode: &IntegerMode) -> bool {
    let Some(expression) = clean_paste(text) else {
        return false;
    };
    let operands = split_operands(&expression, integer::PASTE_CHARACTERS, false);
    !operands.is_empty()
        && operands
            .iter()
            .all(|operand| integer::paste_operand_is_valid(operand, mode.radix, mode.word))
}

/// Splits pasted text into operands at the binary operators
/// (`CopyPasteManager::ExtractOperands`), looking only at the `valid` characters.
/// A sign is not an operator. Empty when there are too many operands or an
/// exponent is too long.
pub(crate) fn extract_operands(expression: &str, valid: &str) -> Vec<String> {
    split_operands(expression, valid, true)
}

/// `extract_operands`; without `exponents` (the Programmer mode, where `e` is a digit)
/// there is no e-notation.
fn split_operands(expression: &str, valid: &str, exponents: bool) -> Vec<String> {
    let chars: Vec<char> = expression.chars().collect();
    let mut operands = Vec::new();
    let mut last_index = 0;
    let mut have_operator = false;
    let mut counting_exponent = false;
    let mut exponent_length = 0;
    let mut start_of_expression = true;
    let mut previous_open_paren = false;
    let mut previous_operator = false;
    for (i, &c) in chars.iter().enumerate() {
        if !valid.contains(c) {
            continue;
        }
        if operands.len() >= MAX_PASTE_OPERANDS {
            return Vec::new();
        }
        if c.is_ascii_digit() {
            if counting_exponent {
                exponent_length += 1;
                if exponent_length > MAX_PASTE_EXPONENT_DIGITS {
                    return Vec::new();
                }
            }
            previous_operator = false;
        } else if c == 'e' {
            counting_exponent |= exponents;
            previous_operator = false;
        } else if "+-*/^%".contains(c) {
            let is_sign = (c == '+' || c == '-')
                && (previous_open_paren
                    || start_of_expression
                    || previous_operator
                    || (exponents && (i == 0 || chars[i - 1] == 'e')));
            if is_sign {
                previous_operator = false;
                continue;
            }
            counting_exponent = false;
            exponent_length = 0;
            have_operator = true;
            previous_operator = true;
            operands.push(chars[last_index..i].iter().collect());
            last_index = i + 1;
        } else {
            previous_operator = false;
        }
        previous_open_paren = c == '(';
        start_of_expression = false;
    }
    if have_operator {
        operands.push(chars[last_index..].iter().collect());
    } else {
        operands = vec![expression.to_string()];
    }
    operands
}

/// Whether an operand is a number: `\s*[-+]?(\d+(\.\d*)?|\.\d+)\s*`, and in the
/// Scientific mode with opening parentheses (each may be signed) before it and
/// closing ones after it, or just an optional sign. This is the converters' pattern,
/// without e-notation.
pub(crate) fn operand_matches(operand: &str, scientific: bool) -> bool {
    number_matches(operand, scientific, false)
}

/// `operand_matches`, optionally with an exponent after the number (`e[+-]?\d+`), as the
/// Standard and Scientific modes accept.
fn number_matches(operand: &str, scientific: bool, exponent: bool) -> bool {
    let mut rest = operand.trim_start();
    if scientific {
        if matches!(rest.trim_end(), "" | "+" | "-") {
            return true;
        }
        while let Some(after) = rest
            .strip_prefix(['+', '-'])
            .unwrap_or(rest)
            .strip_prefix('(')
        {
            rest = after;
        }
        rest = rest.trim_start();
    }
    rest = rest.strip_prefix(['+', '-']).unwrap_or(rest);
    let integer = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    rest = &rest[integer..];
    let mut fraction = 0;
    if let Some(after) = rest.strip_prefix('.') {
        fraction = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        rest = &after[fraction..];
    }
    if integer == 0 && fraction == 0 {
        return false;
    }
    if exponent && let Some(after) = rest.strip_prefix('e') {
        let after = after.strip_prefix(['+', '-']).unwrap_or(after);
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return false;
        }
        rest = &after[digits..];
    }
    if scientific {
        rest = rest.trim_start().trim_start_matches(')');
    }
    rest.trim().is_empty()
}

/// Digits that count towards the operand length limit
/// (`StandardScientificOperandLength` after `SanitizeOperand`): neither the decimal
/// point, nor a leading `0.`, nor the exponent count.
fn operand_length(operand: &str) -> usize {
    let sanitized: String = operand
        .chars()
        .filter(|c| !matches!(c, '\'' | '_' | '`' | '(' | ')' | '-' | '+'))
        .collect();
    let mut length = sanitized.chars().count();
    if sanitized.contains('.') && length >= 2 {
        if sanitized.starts_with("0.") {
            length -= 2;
        } else {
            length -= 1;
        }
    }
    if let Some(at) = sanitized.find('e') {
        length = length.saturating_sub(sanitized.len() - at);
    }
    length
}

// ------------------------------------------------------------- arithmetic

/// `left op right`, with Windows' division errors.
fn apply(left: &Number, op: Op, right: &Number) -> Result<Number, CalcError> {
    let result = match op {
        Op::Add => left + right,
        Op::Subtract => left - right,
        Op::Multiply => left * right,
        Op::Divide if right.is_zero() && left.is_zero() => return Err(CalcError::Undefined),
        Op::Divide if right.is_zero() => return Err(CalcError::DivideByZero),
        Op::Divide => left / right,
        Op::Mod => math::modulo(left, right),
        Op::Power => math::power(left, right)?,
        Op::Root => math::root(left, right)?,
        Op::LogBase => math::log_base(left, right)?,
        // The Programmer mode's operators run in `IntegerMode::apply`.
        _ => return Err(CalcError::InvalidInput),
    };
    normalize(result)
}

/// √(p/q) = √(p·q)/q. Scaling by 10^k keeps at least `INTERNAL_DIGITS` correct
/// digits, and perfect squares (√9, √0.25) give exact results.
fn sqrt(v: &Number) -> Result<Number, CalcError> {
    if v.is_negative() {
        return Err(CalcError::InvalidInput);
    }
    let (p, q) = (v.numer(), v.denom());
    let k = INTERNAL_DIGITS as u32 + (q.bits() / 3) as u32 + 1;
    let root = (p * q * pow10(2 * k as i64)).sqrt();
    normalize(Number::new(root, q * pow10(k as i64)))
}

/// Checks for overflow and keeps the denominator from growing without bound.
fn normalize(v: Number) -> Result<Number, CalcError> {
    if v.is_zero() {
        return Ok(v);
    }
    let magnitude = v.numer().abs();
    let e = exponent(&magnitude, v.denom());
    if e.abs() > MAX_EXPONENT {
        return Err(CalcError::Overflow);
    }
    if v.denom().bits() <= MAX_DENOM_BITS {
        return Ok(v);
    }
    Ok(math::round_digits(&v, INTERNAL_DIGITS))
}

fn pow10(n: i64) -> BigInt {
    BigInt::from(10u8).pow(n as u32)
}

/// Decimal exponent of a/q (a > 0): the e with 10^e ≤ a/q < 10^(e+1).
fn exponent(a: &BigInt, q: &BigInt) -> i64 {
    // Compares a/q with 10^e without dividing.
    let compare = |e: i64| {
        if e >= 0 {
            a.cmp(&(q * pow10(e)))
        } else {
            (a * pow10(-e)).cmp(q)
        }
    };
    let bits = a.bits() as f64 - q.bits() as f64;
    let mut e = (bits * std::f64::consts::LOG10_2).floor() as i64;
    while compare(e) == Ordering::Less {
        e -= 1;
    }
    while compare(e + 1) != Ordering::Less {
        e += 1;
    }
    e
}

/// Rounds a/q · 10^k to an integer (halves away from zero).
fn scaled_round(a: &BigInt, q: &BigInt, k: i64) -> BigInt {
    let (n, d) = if k >= 0 {
        (a * pow10(k), q.clone())
    } else {
        (a.clone(), q * pow10(-k))
    };
    (n * 2 + &d) / (d * 2)
}

// ------------------------------------------------------------- formatting

/// Formats a number like Windows' `NumberToString`: up to `precision` significant
/// digits, with scientific notation for large numbers and for small ones that don't fit
/// into `precision` decimal places, or always (`scientific`, the Scientific mode's
/// F-E), as in `1.5e+3` and `5.e+0`. The expression line has no `grouping`.
fn format_with(v: &Number, grouping: bool, precision: i64, scientific: bool) -> String {
    if v.is_zero() {
        return if scientific { "0.e+0" } else { "0" }.to_string();
    }
    let sign = if v.is_negative() { "-" } else { "" };
    let a = v.numer().abs();
    let q = v.denom();
    let mut e = exponent(&a, q);
    let mut digits = scaled_round(&a, q, precision - 1 - e);
    // Rounding may have added a digit (9.99…96 → 10.00…0).
    if digits >= pow10(precision) {
        digits /= 10;
        e += 1;
    }
    let digits = digits.to_string();

    if !scientific && (0..precision).contains(&e) {
        let (int, frac) = digits.split_at(e as usize + 1);
        let int = if grouping {
            group_thousands(int)
        } else {
            int.to_string()
        };
        let frac = frac.trim_end_matches('0');
        let point = if frac.is_empty() { "" } else { "." };
        return format!("{sign}{int}{point}{frac}");
    }
    if !scientific && e < 0 {
        // Zeros after the decimal point before the first significant digit.
        let zeros = -e - 1;
        if zeros <= MAX_ZEROS_AFTER_DECIMAL {
            // 0.0033333333333333: simply 16 (or 32) digits after the point.
            let frac = scaled_round(&a, q, precision).to_string();
            let frac = format!("{frac:0>width$}", width = precision as usize);
            return format!("{sign}0.{}", frac.trim_end_matches('0'));
        }
        // 0.000125: the whole number fits into 16 (or 32) digits after the point.
        let significant = digits.trim_end_matches('0');
        if zeros + significant.len() as i64 <= precision {
            return format!("{sign}0.{}{significant}", "0".repeat(zeros as usize));
        }
    }

    let (first, rest) = digits.split_at(1);
    let exp_sign = if e >= 0 { "+" } else { "-" };
    format!(
        "{sign}{first}.{}e{exp_sign}{}",
        rest.trim_end_matches('0'),
        e.abs()
    )
}

pub(crate) fn group_thousands(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A typed number: `-12.5`, or with an exponent, `1.5e+3`, `2.e-4`.
fn parse_decimal(input: &str) -> Number {
    let (negative, unsigned) = match input.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, input),
    };
    let (mantissa, exponent) = match unsigned.split_once('e') {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().unwrap_or(0)),
        None => (unsigned, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{int}{frac}");
    let numer: BigInt = if digits.is_empty() {
        BigInt::zero()
    } else {
        digits.parse().unwrap()
    };
    let scale = exponent - frac.len() as i64;
    let v = if scale >= 0 {
        Number::from_integer(numer * pow10(scale))
    } else {
        Number::new(numer, pow10(-scale))
    };
    if negative { -v } else { v }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Presses keys given as a string: digits, `.`, `+-*/`, `=`, `%`, `(`, `)`,
    /// `n` = ±, `s` = x², `r` = √, `i` = 1/x, `c` = C, `e` = CE, `<` = ⌫.
    fn press(calc: &mut Calculator, keys: &str) {
        for key in keys.chars() {
            let command = match key {
                '0'..='9' => Command::Digit(key as u8 - b'0'),
                '.' => Command::Decimal,
                '+' => Command::Operator(Op::Add),
                '-' => Command::Operator(Op::Subtract),
                '*' => Command::Operator(Op::Multiply),
                '/' => Command::Operator(Op::Divide),
                '=' => Command::Equals,
                '%' => Command::Percent,
                '(' => Command::OpenParen,
                ')' => Command::CloseParen,
                'n' => Command::Function(Func::Negate),
                's' => Command::Function(Func::Square),
                'r' => Command::Function(Func::SquareRoot),
                'i' => Command::Function(Func::Reciprocal),
                'c' => Command::Clear,
                'e' => Command::ClearEntry,
                '<' => Command::Backspace,
                ' ' => continue,
                _ => panic!("unknown key {key}"),
            };
            calc.execute(command);
        }
    }

    fn run(keys: &str) -> (String, String) {
        let mut calc = Calculator::default();
        press(&mut calc, keys);
        (calc.display(), calc.expression())
    }

    /// Like `run`, with operator precedence and parentheses (Scientific mode).
    fn run_precedence(keys: &str) -> (String, String) {
        let mut calc = Calculator::with_precedence();
        press(&mut calc, keys);
        (calc.display(), calc.expression())
    }

    /// A number as the Standard mode's display writes it, with thousands separators.
    fn format_number(v: &Number) -> String {
        format_with(v, true, MAX_DIGITS, false)
    }

    fn history(calc: &Calculator) -> Vec<(String, String)> {
        calc.history()
            .iter()
            .map(|e| (e.expression.clone(), format_number(&e.result)))
            .collect()
    }

    #[test]
    fn immediate_execution_without_precedence() {
        assert_eq!(run("2+3*4="), ("20".into(), "5 × 4 =".into()));
    }

    #[test]
    fn chained_operator_shows_intermediate_result() {
        assert_eq!(run("2+3+"), ("5".into(), "5 +".into()));
    }

    #[test]
    fn operator_can_be_replaced() {
        assert_eq!(run("5+*3=").0, "15");
    }

    #[test]
    fn exact_decimal_arithmetic() {
        assert_eq!(run("0.1+0.2=").0, "0.3");
        assert_eq!(run("0.1+0.2-0.3=").0, "0");
        assert_eq!(run("1/3*3=").0, "1");
        assert_eq!(run("1/3*3-1=").0, "0");
    }

    #[test]
    fn subtraction_uses_a_hyphen() {
        assert_eq!(run("9-4="), ("5".into(), "9 - 4 =".into()));
    }

    #[test]
    fn rounding_to_sixteen_digits() {
        assert_eq!(run("1/3=").0, "0.3333333333333333");
        assert_eq!(run("2/3=").0, "0.6666666666666667");
        assert_eq!(run("1/300=").0, "0.0033333333333333");
    }

    #[test]
    fn scientific_notation_for_large_and_small() {
        assert_eq!(run("9999999999999999+1=").0, "1.e+16");
        assert_eq!(run("1234567890123456*1000=").0, "1.234567890123456e+18");
        assert_eq!(run("1/3/1000000000000000/10=").0, "3.333333333333333e-17");
        assert_eq!(run("1/7000000000000000=").0, "1.428571428571429e-16");
        assert_eq!(run("1/3000=").0, "3.333333333333333e-4");
        assert_eq!(run("0.001/8=").0, "0.000125");
        assert_eq!(run("0.0000000000001/8=").0, "0.0000000000000125");
        assert_eq!(run("0.0001/10=").0, "0.00001");
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(run("1234567").0, "1,234,567");
        assert_eq!(run("1234.50").0, "1,234.50");
        assert_eq!(run("1000*1000=").0, "1,000,000");
        assert_eq!(run("1000*1000=").1, "1000 × 1000 =");
        assert_eq!(run("1234567+").1, "1234567 +");
    }

    #[test]
    fn equals_repeats_last_operation() {
        assert_eq!(run("5+3=="), ("11".into(), "8 + 3 =".into()));
        assert_eq!(run("5+3==="), ("14".into(), "11 + 3 =".into()));
        assert_eq!(run("5+="), ("10".into(), "5 + 5 =".into()));
        assert_eq!(run("2+3=7="), ("10".into(), "7 + 3 =".into()));
    }

    #[test]
    fn unary_functions_build_expression() {
        assert_eq!(run("9r"), ("3".into(), "√(9)".into()));
        assert_eq!(run("4ss"), ("256".into(), "sqr(sqr(4))".into()));
        assert_eq!(run("4i"), ("0.25".into(), "1/(4)".into()));
        assert_eq!(run("5+3s"), ("9".into(), "5 + sqr(3)".into()));
        assert_eq!(run("5+3s="), ("14".into(), "5 + sqr(3) =".into()));
        assert_eq!(run("2+3=r").1, "√(5)");
        assert_eq!(run("8=n"), ("-8".into(), "negate(8)".into()));
        // A function right after an operator works on the left operand.
        assert_eq!(run("5+r"), ("2.23606797749979".into(), "5 + √(5)".into()));
    }

    #[test]
    fn square_root_precision() {
        assert_eq!(run("2r").0, "1.414213562373095");
        assert_eq!(run("2rs").0, "2");
        assert_eq!(run("0.25r").0, "0.5");
    }

    #[test]
    fn negate_while_typing() {
        assert_eq!(run("5n"), ("-5".into(), "".into()));
        assert_eq!(run("5nn").0, "5");
        assert_eq!(run("0n").0, "0");
        assert_eq!(run("5n3").0, "-53");
    }

    #[test]
    fn percent_like_windows() {
        assert_eq!(run("50+10%"), ("5".into(), "50 + 5".into()));
        assert_eq!(run("50+10%=").0, "55");
        assert_eq!(run("50*10%=").0, "5");
        assert_eq!(run("50/10%=").0, "500");
        assert_eq!(run("10%"), ("0".into(), "0".into()));
    }

    #[test]
    fn errors() {
        assert_eq!(run("1/0="), ("Cannot divide by zero".into(), "1 ÷".into()));
        assert_eq!(
            run("5+3/0+"),
            ("Cannot divide by zero".into(), "8 ÷ 0 +".into())
        );
        assert_eq!(run("0/0=").0, "Result is undefined");
        assert_eq!(run("0i"), ("Cannot divide by zero".into(), "1/(0)".into()));
        assert_eq!(run("9nr"), ("Invalid input".into(), "√(-9)".into()));
        assert_eq!(run("9999999999999999ssssssssssss").0, "Overflow");
    }

    #[test]
    fn error_blocks_operators_until_digit() {
        let mut calc = Calculator::default();
        press(&mut calc, "1/0=");
        assert!(calc.has_error());
        assert!(!calc.is_enabled(Command::Operator(Op::Add)));
        // As in Windows' number pad, the decimal point is disabled, and `=` isn't.
        assert!(!calc.is_enabled(Command::Decimal));
        assert!(calc.is_enabled(Command::Equals));
        press(&mut calc, "+");
        assert!(calc.has_error());
        press(&mut calc, "7");
        assert!(!calc.has_error());
        assert_eq!((calc.display(), calc.expression()), ("7".into(), "".into()));
        // ⌫ and CE only clear the error.
        press(&mut calc, "1/0=<");
        assert_eq!((calc.display(), calc.has_error()), ("0".into(), false));
    }

    #[test]
    fn digit_limit() {
        assert_eq!(run("12345678901234567890").0, "1,234,567,890,123,456");
        assert_eq!(run("0.12345678901234567").0, "0.1234567890123456");
    }

    #[test]
    fn decimal_point() {
        assert_eq!(run(".5").0, "0.5");
        assert_eq!(run("5..5").0, "5.5");
        assert_eq!(run("5+.").0, "0.");
    }

    #[test]
    fn backspace_and_clear() {
        assert_eq!(run("123<").0, "12");
        assert_eq!(run("1<<").0, "0");
        assert_eq!(run("5n<").0, "0");
        assert_eq!(run("0.<").0, "0");
        assert_eq!(run("2+3=<"), ("5".into(), "".into()));
        // CE after `=` clears the display and the expression line, but like in Windows
        // (`ClearTemporaryValues`) the next `=` still repeats the last operation.
        assert_eq!(run("2+3=e"), ("0".into(), "".into()));
        assert_eq!(run("2+3=e="), ("3".into(), "0 + 3 =".into()));
        assert_eq!(run("5+3e"), ("0".into(), "5 +".into()));
        assert_eq!(run("5+3e2="), ("7".into(), "5 + 2 =".into()));
        assert_eq!(run("5+3c"), ("0".into(), "".into()));
        // CE replaces a function result: `5 + sqr(3)` goes back to `5 +`.
        assert_eq!(run("5+3se"), ("0".into(), "5 +".into()));
    }

    #[test]
    fn new_number_after_result() {
        assert_eq!(run("2+3=4"), ("4".into(), "".into()));
        assert_eq!(run("2+3=*2="), ("10".into(), "5 × 2 =".into()));
    }

    #[test]
    fn memory() {
        let mut calc = Calculator::default();
        assert!(!calc.is_enabled(Command::MemoryRecall));
        press(&mut calc, "5");
        calc.execute(Command::MemoryStore);
        press(&mut calc, "3");
        assert_eq!(calc.display(), "3");
        calc.execute(Command::MemoryAdd);
        calc.execute(Command::MemoryAdd);
        press(&mut calc, "c");
        calc.execute(Command::MemoryRecall);
        assert_eq!(calc.display(), "11");
        calc.execute(Command::MemoryStore);
        assert_eq!(calc.memory().len(), 2);
        calc.memory_clear_item(0);
        assert_eq!(format_number(&calc.memory()[0]), "11");
        calc.execute(Command::MemoryClear);
        assert!(calc.memory().is_empty());
        // M− on an empty memory stores the negative.
        press(&mut calc, "4");
        calc.execute(Command::MemorySubtract);
        assert_eq!(format_number(&calc.memory()[0]), "-4");
    }

    #[test]
    fn history_logs_every_step() {
        let mut calc = Calculator::default();
        press(&mut calc, "1+2+3=");
        assert_eq!(
            history(&calc),
            [
                ("3 + 3 =".to_string(), "6".to_string()),
                ("1 + 2 =".to_string(), "3".to_string())
            ]
        );
        press(&mut calc, "c");
        calc.recall_history(1);
        assert_eq!(
            (calc.display(), calc.expression()),
            ("3".into(), "1 + 2 =".into())
        );
        press(&mut calc, "+1=");
        assert_eq!(calc.display(), "4");
        calc.delete_history_item(0);
        assert_eq!(history(&calc)[0].0, "3 + 3 =");
    }

    #[test]
    fn history_logs_lone_numbers_and_finished_functions() {
        // Windows logs `5 =` too, and a finished `√(4)` when a new number starts.
        let mut calc = Calculator::default();
        press(&mut calc, "5=");
        assert_eq!(history(&calc), [("5 =".to_string(), "5".to_string())]);
        press(&mut calc, "c4r5");
        assert_eq!(history(&calc)[0], ("√(4)".to_string(), "2".to_string()));
    }

    #[test]
    fn history_logs_a_finished_function_with_its_own_result_when_memory_is_recalled() {
        // Source: microsoft/calculator, CalcManager/CEngine/scicomm.cpp, `IDC_RECALL` and
        // `CheckAndAddLastBinOpToHistory`: the history line is completed with the text of the
        // number on the display (`sqr(4)` = 16), not with the recalled value. The same keys on
        // a build of Microsoft's CalcManager (d125246) leave `sqr( 4 )` = `16` in the history,
        // in the Standard and the Scientific mode.
        let mut calc = Calculator::default();
        press(&mut calc, "7");
        calc.execute(Command::MemoryStore);
        press(&mut calc, "4s");
        calc.execute(Command::MemoryRecall);
        assert_eq!(calc.display(), "7");
        assert_eq!(history(&calc), [("sqr(4)".to_string(), "16".to_string())]);

        let mut calc = Calculator::with_precedence();
        press_scientific(&mut calc, "7");
        calc.execute(Command::MemoryStore);
        press_scientific(&mut calc, "4 sqrt");
        calc.execute(Command::MemoryRecall);
        assert_eq!(calc.display(), "7");
        assert_eq!(history(&calc), [("√(4)".to_string(), "2".to_string())]);
    }

    #[test]
    fn standard_mode_has_no_parentheses() {
        let calc = Calculator::default();
        assert!(!calc.is_enabled(Command::OpenParen));
        // The parentheses are ignored: 2 × 3 + 4 =.
        assert_eq!(run("2*(3+4)="), ("10".into(), "6 + 4 =".into()));
    }

    #[test]
    fn precedence_evaluates_higher_operators_first() {
        assert_eq!(
            run_precedence("2+3*4="),
            ("14".into(), "2 + 3 × 4 =".into())
        );
        assert_eq!(
            run_precedence("1+2*3*4="),
            ("25".into(), "1 + 2 × 3 × 4 =".into())
        );
        assert_eq!(run_precedence("8-6/2="), ("5".into(), "8 - 6 ÷ 2 =".into()));
    }

    #[test]
    fn precedence_shows_partial_results() {
        // `+` after `2 + 3 ×` waits; a lower or equal operator computes what it can.
        assert_eq!(run_precedence("2+3*"), ("3".into(), "2 + 3 ×".into()));
        assert_eq!(run_precedence("2*3+"), ("6".into(), "2 × 3 +".into()));
        assert_eq!(
            run_precedence("2+3*4+"),
            ("14".into(), "2 + 3 × 4 +".into())
        );
    }

    #[test]
    fn precedence_inversion_adds_brackets() {
        // 1 + 2 was computed when `-` was pressed; changing `-` to `×` keeps that order.
        assert_eq!(
            run_precedence("1+2-*3="),
            ("9".into(), "(1 + 2) × 3 =".into())
        );
    }

    #[test]
    fn precedence_history_and_repeat() {
        let mut calc = Calculator::with_precedence();
        press(&mut calc, "2+3*4=");
        assert_eq!(
            history(&calc),
            [("2 + 3 × 4 =".to_string(), "14".to_string())]
        );
        // Repeated `=` repeats the last operation done (`+ 12`), as in Windows.
        press(&mut calc, "=");
        assert_eq!(
            (calc.display(), calc.expression()),
            ("26".into(), "14 + 12 =".into())
        );
    }

    #[test]
    fn parentheses() {
        assert_eq!(
            run_precedence("(1+2)*3="),
            ("9".into(), "(1 + 2) × 3 =".into())
        );
        assert_eq!(
            run_precedence("2*(3+4)="),
            ("14".into(), "2 × (3 + 4) =".into())
        );
        assert_eq!(
            run_precedence("((2+3))*2="),
            ("10".into(), "((2 + 3)) × 2 =".into())
        );
        assert_eq!(
            run_precedence("1+(2*3)="),
            ("7".into(), "1 + (2 × 3) =".into())
        );
        assert_eq!(run_precedence("(1+2)s"), ("9".into(), "sqr(1 + 2)".into()));
    }

    #[test]
    fn parentheses_count_and_auto_close() {
        let mut calc = Calculator::with_precedence();
        press(&mut calc, "2*((3+4");
        assert_eq!(calc.open_parentheses(), 2);
        press(&mut calc, "=");
        assert_eq!(calc.open_parentheses(), 0);
        assert_eq!(
            (calc.display(), calc.expression()),
            ("14".into(), "2 × ((3 + 4)) =".into())
        );
    }

    #[test]
    fn parentheses_edge_cases() {
        // `)` without `(` is ignored.
        assert_eq!(run_precedence("2)"), ("2".into(), "".into()));
        // An empty group is 0.
        assert_eq!(run_precedence("1+()="), ("1".into(), "1 + (0) =".into()));
        // A number next to a parenthesis multiplies.
        assert_eq!(run_precedence("2(3)="), ("6".into(), "2 × (3) =".into()));
        assert_eq!(run_precedence("(2)3="), ("6".into(), "(2) × 3 =".into()));
    }

    #[test]
    fn precedence_mode_keeps_percent_and_functions() {
        assert_eq!(run_precedence("50+10%="), ("55".into(), "50 + 5 =".into()));
        assert_eq!(
            run_precedence("2+3s*2="),
            ("20".into(), "2 + sqr(3) × 2 =".into())
        );
    }

    #[test]
    fn paste_numbers() {
        let mut calc = Calculator::default();
        assert!(calc.paste(" 1,234.5 "));
        assert_eq!(calc.display(), "1,234.5");
        assert!(calc.paste("-007"));
        assert_eq!(calc.display(), "-7");
        assert_eq!(calc.copy_text().as_deref(), Some("-7"));
        assert!(calc.paste("$12"));
        assert_eq!(calc.display(), "12");
        assert!(calc.paste("-0.5"));
        assert_eq!(calc.display(), "-0.5");
        // A pasted 0 never takes the sign (`MapCharacterToButtonId`), so the zeros after
        // the point don't flip it back and forth.
        assert!(calc.paste("-0.05"));
        assert_eq!(calc.display(), "-0.05");
    }

    #[test]
    fn paste_expressions() {
        let mut calc = Calculator::default();
        assert!(calc.paste("12 + 5 ="));
        assert_eq!(
            (calc.display(), calc.expression()),
            ("17".into(), "12 + 5 =".into())
        );
        assert!(calc.paste("2*-3="));
        assert_eq!(
            (calc.display(), calc.expression()),
            ("-6".into(), "2 × -3 =".into())
        );
        assert!(calc.paste("2*-0.05="));
        assert_eq!(
            (calc.display(), calc.expression()),
            ("-0.1".into(), "2 × -0.05 =".into())
        );
        // Without `=` the expression waits, like typing it.
        assert!(calc.paste("8/2"));
        assert_eq!(
            (calc.display(), calc.expression()),
            ("2".into(), "8 ÷".into())
        );

        let mut scientific = Calculator::with_precedence();
        assert!(scientific.paste("(1+2)*3="));
        assert_eq!(scientific.display(), "9");
        assert!(scientific.paste("-(2+3)="));
        assert_eq!(scientific.display(), "-5");
    }

    #[test]
    fn invalid_paste_shows_invalid_input() {
        for text in [
            "abc",
            "12345678901234567",
            "5+",
            "(1+2)",
            "1e12345",
            "1e",
            "",
        ] {
            let mut calc = Calculator::default();
            assert!(!calc.paste(text), "{text:?} should be rejected");
            assert_eq!(calc.display(), "Invalid input", "{text:?}");
        }
        assert!(!paste_is_valid(&"1".repeat(MAX_PASTE_LENGTH + 1), false));
        // Digits are typed again after the error.
        let mut calc = Calculator::default();
        calc.paste("abc");
        press(&mut calc, "4");
        assert_eq!(calc.display(), "4");
    }

    #[test]
    fn paste_ignores_keys_after_an_error() {
        // `OnPaste` sends the keys straight to the engine, without the view model's error
        // recovery, and the engine ignores all but C and CE in the error state.
        let mut calc = Calculator::default();
        assert!(calc.paste("1/0*2="));
        assert_eq!(
            (calc.display(), calc.expression()),
            ("Cannot divide by zero".into(), "1 ÷ 0 ×".into())
        );
        assert!(calc.history().is_empty());
        assert!(calc.paste("99e9999+1="));
        assert_eq!(
            (calc.display(), calc.expression()),
            ("Overflow".into(), "9.9e+10000 +".into())
        );
        assert!(calc.history().is_empty());
        // The next paste starts with CE, which is C in the error state.
        assert!(calc.paste("2+3="));
        assert_eq!(calc.display(), "5");
        let mut scientific = Calculator::with_precedence();
        assert!(scientific.paste("(1/0)*2="));
        assert_eq!(scientific.display(), "Cannot divide by zero");
        assert!(scientific.history().is_empty());
    }

    #[test]
    fn paste_keeps_scientific_notation_on() {
        // `OnPaste` sends CE straight to the engine; only the view model's C and CE
        // buttons turn F-E off.
        let mut calc = Calculator::with_precedence();
        calc.execute(Command::ToggleNotation);
        assert!(calc.paste("12+5="));
        assert!(calc.notation());
        assert_eq!(calc.display(), "1.7e+1");
        // In the error state, too.
        assert!(calc.paste("1/0="));
        assert!(calc.paste("3="));
        assert!(calc.notation());
        assert_eq!(calc.display(), "3.e+0");
    }

    #[test]
    fn paste_operand_rules() {
        assert_eq!(
            extract_operands("12+-5", STANDARD_PASTE_CHARACTERS),
            ["12", "-5"]
        );
        assert_eq!(
            extract_operands("-3*4", STANDARD_PASTE_CHARACTERS),
            ["-3", "4"]
        );
        assert!(operand_matches(" -1.5 ", false));
        assert!(operand_matches(".5", false));
        assert!(!operand_matches("1.2.3", false));
        assert!(operand_matches("-(2", true));
        assert!(operand_matches("3))", true));
        assert_eq!(operand_length("0.123"), 3);
        assert_eq!(operand_length("-12.5"), 3);
    }

    // ------------------------------------------------------------ scientific

    /// Presses Scientific mode keys given as words: numbers (`12`, `2.5`),
    /// `+ - * / ( ) =`, `^` (xʸ), `yroot`, `logy` (log base y), `mod`, `neg` (±), the
    /// functions `sqr cube sqrt cbrt inv 10x 2x ex log ln fact abs floor ceil dms todeg`,
    /// the trigonometric ones by name with `a` for the inverse and `h` for hyperbolic
    /// (`asin`, `sinh`, `asinh`), `pi euler rand`, `exp`, `fe` (F-E), `DEG RAD GRAD`,
    /// `c`, `ce` and `bs` (⌫).
    fn press_scientific(calc: &mut Calculator, keys: &str) {
        for key in keys.split_whitespace() {
            let function = |f| Command::Function(f);
            let command = match key {
                "+" => Command::Operator(Op::Add),
                "-" => Command::Operator(Op::Subtract),
                "*" => Command::Operator(Op::Multiply),
                "/" => Command::Operator(Op::Divide),
                "^" => Command::Operator(Op::Power),
                "yroot" => Command::Operator(Op::Root),
                "logy" => Command::Operator(Op::LogBase),
                "mod" => Command::Operator(Op::Mod),
                "(" => Command::OpenParen,
                ")" => Command::CloseParen,
                "=" => Command::Equals,
                "neg" => function(Func::Negate),
                "sqr" => function(Func::Square),
                "cube" => function(Func::Cube),
                "sqrt" => function(Func::SquareRoot),
                "cbrt" => function(Func::CubeRoot),
                "inv" => function(Func::Reciprocal),
                "10x" => function(Func::TenToX),
                "2x" => function(Func::TwoToX),
                "ex" => function(Func::EToX),
                "log" => function(Func::Log),
                "ln" => function(Func::Ln),
                "fact" => function(Func::Factorial),
                "abs" => function(Func::Abs),
                "floor" => function(Func::Floor),
                "ceil" => function(Func::Ceil),
                "dms" => function(Func::Dms),
                "todeg" => function(Func::Degrees),
                "pi" => Command::Pi,
                "euler" => Command::Euler,
                "rand" => Command::Random,
                "exp" => Command::Exponent,
                "fe" => Command::ToggleNotation,
                "DEG" => Command::Angle(Angle::Degrees),
                "RAD" => Command::Angle(Angle::Radians),
                "GRAD" => Command::Angle(Angle::Gradians),
                "c" => Command::Clear,
                "ce" => Command::ClearEntry,
                "bs" => Command::Backspace,
                _ if key.chars().all(|c| c.is_ascii_digit() || c == '.') => {
                    for c in key.chars() {
                        calc.execute(match c {
                            '.' => Command::Decimal,
                            _ => Command::Digit(c as u8 - b'0'),
                        });
                    }
                    continue;
                }
                _ => {
                    let inverse = key.starts_with('a');
                    let name = key.trim_start_matches('a');
                    let hyperbolic = name.len() == 4;
                    let function = TrigFunction::ALL
                        .into_iter()
                        .find(|f| f.label() == &name[..3])
                        .unwrap_or_else(|| panic!("unknown key {key}"));
                    Command::Function(Func::Trig(Trig {
                        function,
                        inverse,
                        hyperbolic,
                    }))
                }
            };
            calc.execute(command);
        }
    }

    fn run_scientific(keys: &str) -> (String, String) {
        let mut calc = Calculator::with_precedence();
        press_scientific(&mut calc, keys);
        (calc.display(), calc.expression())
    }

    fn pair(display: &str, expression: &str) -> (String, String) {
        (display.to_string(), expression.to_string())
    }

    #[test]
    fn scientific_shows_32_digits() {
        assert_eq!(
            run_scientific("1 / 3 =").0,
            "0.33333333333333333333333333333333"
        );
        assert_eq!(
            run_scientific("2 sqrt").0,
            "1.4142135623730950488016887242097"
        );
        // 32 digits can be typed, and the expression line has them all.
        assert_eq!(
            run_scientific("123456789012345678901234567890123456"),
            pair("12,345,678,901,234,567,890,123,456,789,012", "")
        );
        assert_eq!(
            run_scientific("1 / 3 = +").1,
            "0.33333333333333333333333333333333 +"
        );
        // The Standard mode keeps its 16.
        assert_eq!(run("1/3=").0, "0.3333333333333333");
    }

    #[test]
    fn scientific_binary_operators() {
        assert_eq!(run_scientific("2 ^ 10 ="), pair("1,024", "2 ^ 10 ="));
        // xʸ binds tighter than × and ÷.
        assert_eq!(run_scientific("2 * 3 ^ 2 =").0, "18");
        assert_eq!(run_scientific("2 ^ 3 * 2 =").0, "16");
        assert_eq!(run_scientific("8 yroot 3 ="), pair("2", "8 yroot 3 ="));
        assert_eq!(run_scientific("8 logy 2 ="), pair("3", "8 log base 2 ="));
        assert_eq!(run_scientific("10 mod 3 ="), pair("1", "10 Mod 3 ="));
        // The remainder has the sign of the divisor, and x mod 0 is x.
        assert_eq!(run_scientific("10 neg mod 3 =").0, "2");
        assert_eq!(run_scientific("10 mod 3 neg =").0, "-2");
        assert_eq!(run_scientific("7 mod 0 =").0, "7");
        assert_eq!(run_scientific("2 neg ^ 0.5 =").0, "Invalid input");
        assert_eq!(run_scientific("0 ^ 1 neg =").0, "Invalid input");
    }

    #[test]
    fn scientific_functions_in_the_expression_line() {
        assert_eq!(run_scientific("30 sin"), pair("0.5", "sin₀(30)"));
        assert_eq!(run_scientific("RAD 0 cos"), pair("1", "cosᵣ(0)"));
        assert_eq!(run_scientific("GRAD 100 sin"), pair("1", "sin₉(100)"));
        assert_eq!(run_scientific("0.5 asin"), pair("30", "sin₀⁻¹(0.5)"));
        assert_eq!(run_scientific("0 sinh"), pair("0", "sinh(0)"));
        assert_eq!(run_scientific("0 atanh"), pair("0", "tanh⁻¹(0)"));
        assert_eq!(run_scientific("60 sec"), pair("2", "sec₀(60)"));
        assert_eq!(run_scientific("2 asec"), pair("60", "sec₀⁻¹(2)"));
        assert_eq!(run_scientific("1000 log"), pair("3", "log(1000)"));
        assert_eq!(run_scientific("1 ln"), pair("0", "ln(1)"));
        assert_eq!(run_scientific("5 fact"), pair("120", "fact(5)"));
        assert_eq!(run_scientific("2 cube"), pair("8", "cube(2)"));
        assert_eq!(run_scientific("27 cbrt"), pair("3", "cuberoot(27)"));
        assert_eq!(run_scientific("3 10x"), pair("1,000", "10^(3)"));
        assert_eq!(run_scientific("3 2x"), pair("8", "2^(3)"));
        assert_eq!(
            run_scientific("1 ex"),
            pair("2.7182818284590452353602874713527", "e^(1)")
        );
        assert_eq!(run_scientific("5 neg abs"), pair("5", "abs(-5)"));
        assert_eq!(run_scientific("2.5 neg floor"), pair("-3", "floor(-2.5)"));
        assert_eq!(run_scientific("2.5 neg ceil"), pair("-2", "ceil(-2.5)"));
        assert_eq!(run_scientific("30.5 dms"), pair("30.3", "dms(30.5)"));
        assert_eq!(run_scientific("30.3 todeg"), pair("30.5", "degrees(30.3)"));
        // Functions nest and apply to the operand after an operator.
        assert_eq!(run_scientific("2 + 9 sqrt ="), pair("5", "2 + √(9) ="));
        assert_eq!(run_scientific("90 sin sqr"), pair("1", "sqr(sin₀(90))"));
    }

    #[test]
    fn scientific_errors() {
        assert_eq!(run_scientific("90 tan"), pair("Invalid input", "tan₀(90)"));
        assert_eq!(run_scientific("0 ln").0, "Invalid input");
        assert_eq!(run_scientific("4 neg sqrt").0, "Invalid input");
        assert_eq!(run_scientific("3 neg fact").0, "Invalid input");
        assert_eq!(run_scientific("3250 fact").0, "Overflow");
        assert_eq!(run_scientific("0 csc").0, "Cannot divide by zero");
        assert_eq!(run_scientific("1 atanh").0, "Cannot divide by zero");
        assert_eq!(run_scientific("2 asin").0, "Invalid input");
        // Windows refuses trigonometry from 10^100 on, even sinh.
        assert_eq!(
            run_scientific("1 exp 100 sinh"),
            pair("Invalid input", "sinh(1.e+100)")
        );
        // In the error state the functions are disabled, but not F-E and the angle unit
        // (as in Windows); they clear the error and apply.
        let mut calc = Calculator::with_precedence();
        press_scientific(&mut calc, "0 ln");
        assert!(!calc.is_enabled(Command::Function(Func::Log)));
        assert!(!calc.is_enabled(Command::Pi));
        assert!(calc.is_enabled(Command::ToggleNotation));
        press_scientific(&mut calc, "RAD");
        assert_eq!((calc.display(), calc.angle()), ("0".into(), Angle::Radians));
        press_scientific(&mut calc, "0 ln 7");
        assert_eq!(calc.display(), "7");
    }

    #[test]
    fn scientific_constants() {
        assert_eq!(
            run_scientific("pi"),
            pair("3.1415926535897932384626433832795", "")
        );
        assert_eq!(
            run_scientific("euler").0,
            "2.7182818284590452353602874713527"
        );
        // π replaces the typed number: no multiplication, as in Windows.
        assert_eq!(run_scientific("2 pi").0, run_scientific("pi").0);
        assert_eq!(
            run_scientific("5 + pi ="),
            pair(
                "8.1415926535897932384626433832795",
                "5 + 3.1415926535897932384626433832795 ="
            )
        );
        assert_eq!(run_scientific("pi ( 2 ) ="), pair("2", "(2) ="));
        let mut calc = Calculator::with_precedence();
        press_scientific(&mut calc, "rand");
        let shown = calc.display();
        assert!(shown.starts_with("0.") || shown == "0", "{shown}");
    }

    #[test]
    fn exponent_entry() {
        assert_eq!(run_scientific("1.5 exp 3"), pair("1.5e+3", ""));
        assert_eq!(run_scientific("1.5 exp 3 +"), pair("1,500", "1500 +"));
        // `exp` adds a decimal point; ± flips the exponent's sign.
        assert_eq!(run_scientific("2 exp 3 neg"), pair("2.e-3", ""));
        assert_eq!(run_scientific("2 exp 3 neg =").0, "0.002");
        assert_eq!(run_scientific("exp 5"), pair("0.e+5", ""));
        // The exponent has at most 4 digits and no decimal point.
        assert_eq!(run_scientific("1 exp 123456").0, "1.e+1234");
        assert_eq!(run_scientific("1 exp 1.2").0, "1.e+12");
        // ⌫ removes the exponent's digits, then the exponent.
        assert_eq!(run_scientific("5 exp 12 bs").0, "5.e+1");
        assert_eq!(run_scientific("5 exp 12 bs bs").0, "5.e+0");
        assert_eq!(run_scientific("5 exp 12 bs bs bs").0, "5.");
        // `exp` does nothing to a finished number.
        assert_eq!(run_scientific("5 + exp 3 =").0, "8");
        // A typed number beyond Windows' 4-digit exponents overflows.
        assert_eq!(
            run_scientific("99 exp 9999 +"),
            pair("Overflow", "9.9e+10000 +")
        );
    }

    #[test]
    fn scientific_notation_toggle() {
        assert_eq!(run_scientific("fe").0, "0.e+0");
        assert_eq!(run_scientific("12345 fe").0, "1.2345e+4");
        // While typing, the number shows as typed; once finished, in e-notation, in the
        // expression line too.
        assert_eq!(run_scientific("fe 5").0, "5");
        assert_eq!(run_scientific("fe 5 +"), pair("5.e+0", "5.e+0 +"));
        assert_eq!(run_scientific("fe 0.5 =").0, "5.e-1");
        // C and CE turn it off, as Windows' view model does.
        let mut calc = Calculator::with_precedence();
        press_scientific(&mut calc, "fe 2 + 3 =");
        assert!(calc.notation());
        assert_eq!(calc.history()[0].text, "5.e+0");
        press_scientific(&mut calc, "ce");
        assert!(!calc.notation());
        press_scientific(&mut calc, "fe c");
        assert!(!calc.notation());
        // Switching modes turns it off too.
        press_scientific(&mut calc, "fe");
        calc.reset();
        assert!(!calc.notation());
    }

    #[test]
    fn angle_units() {
        let mut calc = Calculator::with_precedence();
        assert_eq!(calc.angle(), Angle::Degrees);
        // A new unit finishes the typed number.
        press_scientific(&mut calc, "30 RAD");
        assert_eq!(calc.angle(), Angle::Radians);
        press_scientific(&mut calc, "sin");
        assert_eq!(calc.expression(), "sinᵣ(30)");
        assert_eq!(calc.display(), "-0.98803162409286178998774890729446");
        // The unit stays when the mode starts over.
        calc.reset();
        assert_eq!(calc.angle(), Angle::Radians);
    }

    #[test]
    fn degrees_hide_the_expression_after_equals() {
        // `→deg` sends `IDC_INV` through Windows' engine, which clears the expression
        // shown after `=`.
        assert_eq!(run_scientific("2 + 3 = todeg"), pair("5", ""));
        assert_eq!(run_scientific("5 todeg"), pair("5", "degrees(5)"));
        // It isn't a function to Windows: a new number replaces it instead of logging it.
        let mut calc = Calculator::with_precedence();
        press_scientific(&mut calc, "30.3 todeg 2");
        assert!(calc.history().is_empty());
        press_scientific(&mut calc, "c 4 sqrt 2");
        assert_eq!(calc.history()[0].expression, "√(4)");
    }

    #[test]
    fn scientific_history_is_its_own() {
        let mut calc = Calculator::with_precedence();
        press_scientific(&mut calc, "2 + 3 ^ 2 =");
        assert_eq!(calc.history()[0].expression, "2 + 3 ^ 2 =");
        assert_eq!(calc.history()[0].text, "11");
        assert_eq!(calc.display(), "11");
    }

    #[test]
    fn scientific_keys_only_in_the_scientific_mode() {
        let standard = Calculator::default();
        assert!(!standard.is_enabled(Command::Function(Func::Log)));
        assert!(!standard.is_enabled(Command::Operator(Op::Power)));
        assert!(!standard.is_enabled(Command::Operator(Op::Mod)));
        assert!(!standard.is_enabled(Command::Pi));
        // `exp` exists for paste.
        assert!(standard.is_enabled(Command::Exponent));
        let programmer = Calculator::programmer();
        assert!(!programmer.is_enabled(Command::Operator(Op::Power)));
        assert!(!programmer.is_enabled(Command::Exponent));
        assert!(!programmer.is_enabled(Command::ToggleNotation));
        let scientific = Calculator::with_precedence();
        assert!(scientific.is_enabled(Command::Operator(Op::Mod)));
        assert!(!scientific.is_enabled(Command::Operator(Op::And)));
    }

    #[test]
    fn paste_e_notation_and_scientific_operators() {
        let mut calc = Calculator::default();
        assert!(calc.paste("1e5"));
        assert_eq!(calc.display(), "1.e+5");
        assert!(calc.paste("1.5e-3="));
        assert_eq!(calc.display(), "0.0015");
        assert!(calc.paste("2e+3+1="));
        assert_eq!(calc.display(), "2,001");
        // `^` and `%` are the Scientific mode's.
        assert!(!calc.paste("2^10"));
        let mut scientific = Calculator::with_precedence();
        assert!(scientific.paste("2^10="));
        assert_eq!(scientific.display(), "1,024");
        assert!(scientific.paste("10%3="));
        assert_eq!(scientific.display(), "1");
        assert!(scientific.paste("-2e-1*5="));
        assert_eq!(scientific.display(), "-1");
        assert_eq!(operand_length("1.5e-12"), 2);
    }

    #[test]
    fn scientific_power_of_a_number_close_to_one() {
        // Windows' `exprat` checks y·ln x itself: so close to 1 an estimate of ln x is 0,
        // but the product is beyond ±100000 (here 10^23, whose e^x never ended).
        assert_eq!(
            run_scientific("1.00000000000000001 ^ ( 1 exp 40 + .5 ) =").0,
            "Invalid input"
        );
        let mut calc = Calculator::with_precedence();
        assert!(calc.paste("1.00000000000000001^(1e40+.5)="));
        assert_eq!(calc.display(), "Invalid input");
    }

    // ------------------------------------------------------------ programmer

    /// Presses Programmer mode keys given as words: digits in upper case (`1F`),
    /// `+ - * / % ( ) =`, `and or xor nand nor lsh rsh rshl not rol ror rolc rorc`,
    /// `neg` (±), `c`, `ce`, `bs` (⌫), `hex dec oct bin`, `qword dword word byte`,
    /// and `bit<n>` to flip a bit.
    fn press_programmer(calc: &mut Calculator, keys: &str) {
        for key in keys.split_whitespace() {
            let command = match key {
                "+" => Command::Operator(Op::Add),
                "-" => Command::Operator(Op::Subtract),
                "*" => Command::Operator(Op::Multiply),
                "/" => Command::Operator(Op::Divide),
                "%" => Command::Operator(Op::Mod),
                "and" => Command::Operator(Op::And),
                "or" => Command::Operator(Op::Or),
                "xor" => Command::Operator(Op::Xor),
                "nand" => Command::Operator(Op::Nand),
                "nor" => Command::Operator(Op::Nor),
                "lsh" => Command::Operator(Op::LeftShift),
                "rsh" => Command::Operator(Op::RightShift),
                "rshl" => Command::Operator(Op::RightShiftLogical),
                "not" => Command::Function(Func::Not),
                "rol" => Command::Function(Func::RotateLeft),
                "ror" => Command::Function(Func::RotateRight),
                "rolc" => Command::Function(Func::RotateLeftCarry),
                "rorc" => Command::Function(Func::RotateRightCarry),
                "neg" => Command::Function(Func::Negate),
                "(" => Command::OpenParen,
                ")" => Command::CloseParen,
                "=" => Command::Equals,
                "c" => Command::Clear,
                "ce" => Command::ClearEntry,
                "bs" => Command::Backspace,
                "hex" => Command::Radix(Radix::Hex),
                "dec" => Command::Radix(Radix::Decimal),
                "oct" => Command::Radix(Radix::Octal),
                "bin" => Command::Radix(Radix::Binary),
                "qword" => Command::WordSize(WordSize::Qword),
                "dword" => Command::WordSize(WordSize::Dword),
                "word" => Command::WordSize(WordSize::Word),
                "byte" => Command::WordSize(WordSize::Byte),
                _ if key.starts_with("bit") => Command::ToggleBit(key[3..].parse().unwrap()),
                _ if key
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c)) =>
                {
                    for c in key.chars() {
                        calc.execute(Command::Digit(c.to_digit(16).unwrap() as u8));
                    }
                    continue;
                }
                _ => panic!("unknown key {key}"),
            };
            calc.execute(command);
        }
    }

    fn run_programmer(keys: &str) -> (String, String) {
        let mut calc = Calculator::programmer();
        press_programmer(&mut calc, keys);
        (calc.display(), calc.expression())
    }

    fn radix_values(calc: &Calculator) -> [String; 4] {
        calc.radix_values().unwrap()
    }

    #[test]
    fn programmer_bitwise_operators() {
        assert_eq!(
            run_programmer("12 and 10 ="),
            ("8".into(), "12 AND 10 =".into())
        );
        assert_eq!(run_programmer("12 or 10 =").0, "14");
        assert_eq!(run_programmer("12 xor 10 =").0, "6");
        assert_eq!(run_programmer("byte 12 nand 10 =").0, "-9");
        assert_eq!(run_programmer("byte 12 nor 10 =").0, "-15");
        assert_eq!(run_programmer("bin 1010 and 1100 =").0, "1000");
        // NOT respects the word size.
        assert_eq!(
            run_programmer("byte hex 0 not"),
            ("FF".into(), "NOT(0)".into())
        );
        assert_eq!(
            run_programmer("5 lsh 2 = not"),
            ("-21".into(), "NOT(20)".into())
        );
    }

    #[test]
    fn programmer_precedence() {
        // OR and XOR bind loosest, then AND, then + and −, then × ÷ % and the shifts.
        assert_eq!(
            run_programmer("4 or 2 + 2 ="),
            ("4".into(), "4 OR 2 + 2 =".into())
        );
        assert_eq!(run_programmer("8 or 6 and 3 =").0, "10");
        assert_eq!(run_programmer("1 + 1 lsh 3 =").0, "9");
        assert_eq!(run_programmer("2 + 3 * 4 =").0, "14");
        assert_eq!(run_programmer("( 1 + 2 ) * 3 =").0, "9");
    }

    #[test]
    fn programmer_integer_division() {
        assert_eq!(run_programmer("7 / 2 =").0, "3");
        // The display truncates every result: 5 ÷ 2 × 2 is 4.
        assert_eq!(run_programmer("5 / 2 * 2 =").0, "4");
        assert_eq!(run_programmer("7 neg / 2 =").0, "-3");
        assert_eq!(run_programmer("7 % 3 =").0, "1");
        assert_eq!(run_programmer("7 neg % 3 =").0, "-1");
        assert_eq!(run_programmer("7 % 3 neg =").0, "1");
        assert_eq!(run_programmer("5 / 0 =").0, "Cannot divide by zero");
        assert_eq!(run_programmer("0 / 0 =").0, "Result is undefined");
        assert_eq!(run_programmer("5 % 0 =").0, "Result is undefined");
        // Inside parentheses a quotient keeps its fraction until the parentheses
        // close: 10 − 3.5 = 6.5, and 255 AND 6.5 is 6.
        assert_eq!(
            run_programmer("( 255 and 10 - 7 / 2 ) ="),
            ("6".into(), "(255 AND 10 - 7 ÷ 2) =".into())
        );
    }

    #[test]
    fn programmer_shifts_and_rotations() {
        assert_eq!(
            run_programmer("5 lsh 2 ="),
            ("20".into(), "5 Lsh 2 =".into())
        );
        assert_eq!(run_programmer("byte bin 10000000 rsh 1 =").0, "1100 0000");
        assert_eq!(run_programmer("byte bin 10000000 rshl 1 =").0, "0100 0000");
        assert_eq!(run_programmer("bin 101 rshl 1 =").0, "0010");
        assert_eq!(
            run_programmer("byte bin 10000001 rol"),
            ("0011".into(), "RoL(10000001)".into())
        );
        assert_eq!(run_programmer("byte bin 10000001 ror").0, "1100 0000");
        // A shift by the word size or more has no result.
        assert_eq!(
            run_programmer("1 lsh 64 ="),
            ("Result not defined".into(), "1 Lsh".into())
        );
        assert_eq!(run_programmer("byte 1 lsh 8 =").0, "Result not defined");
        // Through carry, the bit that falls out comes back in on the next rotation;
        // C clears the carry.
        let mut calc = Calculator::programmer();
        press_programmer(&mut calc, "byte hex 80 rolc");
        assert_eq!(calc.display(), "0");
        press_programmer(&mut calc, "rolc");
        assert_eq!(calc.display(), "1");
        press_programmer(&mut calc, "rorc rorc");
        assert_eq!(calc.display(), "80");
        press_programmer(&mut calc, "c 1 rorc rolc");
        assert_eq!(calc.display(), "1");
    }

    #[test]
    fn programmer_negative_numbers() {
        let mut calc = Calculator::programmer();
        press_programmer(&mut calc, "5 neg");
        assert_eq!(calc.display(), "-5");
        assert_eq!(
            radix_values(&calc),
            [
                "FFFF FFFF FFFF FFFB".to_string(),
                "-5".into(),
                "1 777 777 777 777 777 777 773".into(),
                "1111 1111 1111 1111 1111 1111 1111 1111 1111 1111 1111 1111 1111 1111 1111 1011"
                    .into()
            ]
        );
        // ± outside decimal works on the finished number.
        assert_eq!(
            run_programmer("hex 5 neg"),
            ("FFFF FFFF FFFF FFFB".into(), "negate(5)".into())
        );
        // A typed negative number can't become a positive one that doesn't fit.
        let mut calc = Calculator::programmer();
        press_programmer(&mut calc, "byte 12 neg 8 neg");
        assert_eq!(calc.display(), "-128");
        press_programmer(&mut calc, "+");
        assert_eq!(calc.display(), "-128");
    }

    #[test]
    fn programmer_radix_change_rewrites_the_expression() {
        // The operands are written again from their unsigned values, so -5 comes
        // back as 18446744073709551611 in decimal, as in Windows.
        let mut calc = Calculator::programmer();
        press_programmer(&mut calc, "5 neg + 2");
        assert_eq!(
            (calc.display(), calc.expression()),
            ("2".into(), "-5 +".into())
        );
        press_programmer(&mut calc, "hex");
        assert_eq!(calc.expression(), "FFFFFFFFFFFFFFFB +");
        press_programmer(&mut calc, "dec");
        assert_eq!(calc.expression(), "18446744073709551611 +");
        press_programmer(&mut calc, "=");
        assert_eq!(calc.display(), "-3");
        // After `=`, a new radix clears the expression line.
        press_programmer(&mut calc, "hex");
        assert_eq!(
            (calc.display(), calc.expression()),
            ("FFFF FFFF FFFF FFFD".into(), "".into())
        );
        // A number being typed is finished by a new radix.
        assert_eq!(run_programmer("12 hex"), ("C".into(), "".into()));
        assert_eq!(run_programmer("12 hex 3"), ("3".into(), "".into()));
    }

    #[test]
    fn programmer_digit_limits() {
        assert_eq!(
            run_programmer("hex FFFFFFFFFFFFFFFFF").0,
            "FFFF FFFF FFFF FFFF"
        );
        assert_eq!(
            run_programmer("hex FFFFFFFFFFFFFFFF").0,
            "FFFF FFFF FFFF FFFF"
        );
        assert_eq!(
            run_programmer("9223372036854775807").0,
            "9,223,372,036,854,775,807"
        );
        assert_eq!(
            run_programmer("9223372036854775808").0,
            "922,337,203,685,477,580"
        );
        assert_eq!(run_programmer("byte 255").0, "25");
        assert_eq!(run_programmer("byte oct 777").0, "77");
        assert_eq!(run_programmer("byte oct 377").0, "377");
        assert_eq!(
            run_programmer("word bin 11111111111111111").0,
            "1111 1111 1111 1111"
        );
    }

    #[test]
    fn programmer_word_sizes() {
        // A smaller word keeps the low bits; a larger one extends the sign.
        assert_eq!(run_programmer("hex 1FFFF word").0, "FFFF");
        assert_eq!(run_programmer("hex FF byte").0, "FF");
        assert_eq!(run_programmer("hex FF byte word").0, "FFFF");
        assert_eq!(run_programmer("hex 7F byte qword").0, "7F");
        assert_eq!(run_programmer("200 byte").0, "-56");
        assert_eq!(run_programmer("byte 100 + 100 =").0, "-56");
    }

    #[test]
    fn programmer_bit_toggling() {
        let mut calc = Calculator::programmer();
        press_programmer(&mut calc, "bit0 bit63");
        assert_eq!(calc.display(), "-9,223,372,036,854,775,807");
        assert_eq!(calc.bits(), 0x8000_0000_0000_0001);
        press_programmer(&mut calc, "bit63");
        assert_eq!(calc.display(), "1");
        // After an operator, a bit flips the left operand, like a function.
        assert_eq!(
            run_programmer("5 + bit1 ="),
            ("12".into(), "5 + 7 =".into())
        );
        // A typed number is finished first; bits beyond the word do nothing.
        assert_eq!(run_programmer("5 bit3").0, "13");
        assert_eq!(run_programmer("byte bit8").0, "0");
    }

    #[test]
    fn programmer_keys_and_errors() {
        let mut calc = Calculator::programmer();
        assert!(!calc.is_enabled(Command::Digit(10)));
        assert!(!calc.is_enabled(Command::Decimal));
        assert!(!calc.is_enabled(Command::Function(Func::SquareRoot)));
        press_programmer(&mut calc, "hex");
        assert!(calc.is_enabled(Command::Digit(15)));
        press_programmer(&mut calc, "bin");
        assert!(!calc.is_enabled(Command::Digit(2)));
        assert!(calc.is_enabled(Command::Digit(1)));
        // In the error state `=` and the radix list stay enabled, the operators don't.
        press_programmer(&mut calc, "dec 1 / 0 =");
        assert!(calc.has_error());
        assert!(calc.radix_values().unwrap().iter().all(String::is_empty));
        assert!(calc.is_enabled(Command::Equals));
        assert!(!calc.is_enabled(Command::Operator(Op::And)));
        assert!(!calc.is_enabled(Command::WordSize(WordSize::Byte)));
        press_programmer(&mut calc, "=");
        assert_eq!((calc.display(), calc.has_error()), ("0".into(), false));
        press_programmer(&mut calc, "1 / 0 = hex");
        assert_eq!(
            (calc.display(), calc.radix()),
            ("0".into(), Some(Radix::Hex))
        );
        // The Standard mode has none of these keys.
        let standard = Calculator::default();
        assert!(!standard.is_enabled(Command::Operator(Op::And)));
        assert!(!standard.is_enabled(Command::Digit(10)));
        assert!(!standard.is_enabled(Command::Radix(Radix::Hex)));
    }

    #[test]
    fn programmer_clear_or_clear_entry() {
        // The keypad shows C when nothing has been typed and the display is 0.
        let mut calc = Calculator::programmer();
        assert!(calc.is_input_empty());
        press_programmer(&mut calc, "0");
        assert!(calc.is_input_empty());
        press_programmer(&mut calc, "5");
        assert!(!calc.is_input_empty());
        press_programmer(&mut calc, "+");
        assert!(!calc.is_input_empty());
        press_programmer(&mut calc, "ce");
        assert!(calc.is_input_empty());
    }

    #[test]
    fn programmer_memory_and_history() {
        let mut calc = Calculator::programmer();
        press_programmer(&mut calc, "byte 1 neg");
        calc.execute(Command::MemoryStore);
        assert_eq!(calc.memory()[0], Number::from_integer(255.into()));
        assert_eq!(calc.format_value(&calc.memory()[0]), "-1");
        calc.execute(Command::MemoryAdd);
        assert_eq!(calc.format_value(&calc.memory()[0]), "-2");
        press_programmer(&mut calc, "hex");
        assert_eq!(calc.format_value(&calc.memory()[0]), "FE");
        // The Programmer mode keeps no history.
        press_programmer(&mut calc, "1 + 2 =");
        assert!(calc.history().is_empty());
    }

    #[test]
    fn programmer_reset() {
        let mut calc = Calculator::programmer();
        press_programmer(&mut calc, "word hex 5 +");
        calc.execute(Command::MemoryStore);
        calc.reset();
        assert_eq!((calc.display(), calc.expression()), ("0".into(), "".into()));
        assert_eq!(calc.radix(), Some(Radix::Decimal));
        assert_eq!(calc.word_size(), Some(WordSize::Word));
        assert_eq!(calc.memory().len(), 1);
    }

    #[test]
    fn programmer_paste() {
        let mut calc = Calculator::programmer();
        press_programmer(&mut calc, "hex");
        assert!(calc.paste("0x1F + 1 ="));
        assert_eq!(
            (calc.display(), calc.expression()),
            ("20".into(), "1F + 1 =".into())
        );
        assert!(calc.paste("ffh"));
        assert_eq!(calc.display(), "FF");
        assert!(calc.paste("A % 3 ="));
        assert_eq!(calc.display(), "1");
        assert!(!calc.paste("12.5"));
        assert_eq!(calc.display(), "Invalid input");

        let mut calc = Calculator::programmer();
        assert!(calc.paste("-5"));
        assert_eq!(calc.display(), "-5");
        assert!(calc.paste("1_000"));
        assert_eq!(calc.display(), "1,000");
        assert!(!calc.paste("FF"));
        assert!(!calc.paste("9223372036854775808"));
        assert!(calc.paste("-9223372036854775808"));
        assert_eq!(calc.display(), "-9,223,372,036,854,775,808");

        let mut calc = Calculator::programmer();
        press_programmer(&mut calc, "bin");
        assert!(calc.paste("0b1010"));
        assert_eq!(calc.display(), "1010");
        assert!(calc.copy_text().is_some_and(|text| text == "1010"));
        press_programmer(&mut calc, "c 111111111");
        assert_eq!(calc.copy_text().as_deref(), Some("000111111111"));
    }
}
