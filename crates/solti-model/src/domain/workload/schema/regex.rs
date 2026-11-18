//! # Schema regexes
//!
//! Podium compiles each `pattern` and each `patternProperties` key with Go's
//! RE2 parser, after bounding it (`internal/workloadschema/regex.go` at commit
//! a1b7ee4). A regex is checked here in Podium's order, and refused if any of
//! these refuse it:
//!
//! 1. **Its source:** at most [`MAX_SCHEMA_REGEX_SOURCE_BYTES`] bytes, and
//!    [`MAX_SCHEMA_REGEX_SOURCE_BYTES_TOTAL`] for all the regexes of a schema.
//! 2. **Podium's raw-text scans** (its `scanPattern`), in its order:
//!    - `(?` followed by `i`, `m`, `s`, `U` or `-` anywhere is refused, as an
//!      inline flag group, even where it is none, such as in `[(?i)]` or
//!      `\(?id`. So Go never folds case for a regex Podium accepts;
//!    - every `\p` or `\P` in the source counts as a Unicode class, even one
//!      that is not, such as the `\p` of `\\p`. Podium counts the larger of
//!      this raw count and its structural scan's, which is always the raw one:
//!      every class the scan finds starts with a raw `\p` or `\P`. At most
//!      [`MAX_SCHEMA_REGEX_UNICODE_ESCAPES`] for a regex (`maxUnicodeClasses`),
//!      and [`MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL`] for a schema: Podium
//!      charges each class 833 ranges of parse work (`unicodeClassWork`),
//!      within 128 000 for all (`maxSchemaPatternWork`).
//! 3. **Its syntax:** it must parse with `regex-syntax` and be read the same way
//!    by Go. Look-around, back-references, `\u`, `\Z`, flags, counts over
//!    1 000 and stacked repetitions are refused, as Go refuses them; the
//!    syntax Rust and Go read differently is refused as SDK-strict
//!    ([`SdkStrictRegexRule`]), and so is every Unicode property class, which
//!    only Go's Unicode tables can size.
//! 4. **Its size:** Go's parse tree, rebuilt here from the syntax tree as Go's
//!    parser builds it (literal merging, the classes Go rewrites as literals,
//!    the factoring of alternations), measured as Podium's `regexSize`
//!    measures it: each class counts its ranges. At most
//!    [`MAX_SCHEMA_REGEX_INSTRUCTIONS`] for a regex, and
//!    [`MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL`] for a schema.
//!
//! The rebuilt tree is Go's for every regex accepted here, with one exception
//! that only counts higher: a class of two non-ASCII runes that fold to each
//! other, such as `[Δδ]`, stays a class here, where Go rewrites it as a
//! case-folded literal.

use regex_syntax::ast::{self, Ast};

use super::{
    MAX_SCHEMA_REGEX_INSTRUCTIONS, MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL,
    MAX_SCHEMA_REGEX_SOURCE_BYTES, MAX_SCHEMA_REGEX_SOURCE_BYTES_TOTAL,
    MAX_SCHEMA_REGEX_UNICODE_ESCAPES, MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL, SdkStrictRegexRule,
    WorkloadSchemaErrorReason as Reason,
};

/// The parse work Podium charges for each Unicode class (`unicodeClassWork`):
/// the most ranges Go 1.27's parser appends for one `\p` or `\P`.
const UNICODE_CLASS_WORK: usize = 833;
/// Podium's bound on the parse work of a schema's regexes (`maxSchemaPatternWork`).
const MAX_SCHEMA_PATTERN_WORK: usize = 128_000;

// The schema's limit on classes is Podium's work bound divided by the work of
// one class: 153 × 833 = 127 449, and 154 × 833 = 128 282.
const _: () = {
    assert!(MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL * UNICODE_CLASS_WORK <= MAX_SCHEMA_PATTERN_WORK);
    assert!(
        (MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL + 1) * UNICODE_CLASS_WORK > MAX_SCHEMA_PATTERN_WORK
    );
};

/// How deep `regex-syntax` may nest: a regex within
/// [`MAX_SCHEMA_REGEX_SOURCE_BYTES`] never nests deeper, as Go never refuses
/// one for its height either.
const NEST_LIMIT: u32 = 1_024;

const _: () = assert!(NEST_LIMIT as usize >= MAX_SCHEMA_REGEX_SOURCE_BYTES);

/// Go's limit on a repetition count, and on nested counts multiplied together.
const MAX_REPEAT: u32 = 1_000;

/// The largest rune.
const MAX_RUNE: u32 = 0x0010_FFFF;

/// Sizes stop past every bound, so that no product overflows (Podium's
/// `saturated`).
const SATURATED: usize = MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL + 1;

const INLINE_FLAGS: &str = "it uses an inline flag group, such as (?i), (?m), (?s), (?U), \
     (?-i) or (?i:…), which Podium refuses wherever its text appears";

/// What the regexes of one schema have taken of their bounds so far.
#[derive(Debug, Default)]
pub(super) struct RegexBudget {
    bytes: usize,
    unicode_escapes: usize,
    instructions: usize,
}

impl RegexBudget {
    /// Checks one regex of the schema and charges it to the schema's bounds.
    pub(super) fn charge(&mut self, pattern: &str) -> Result<(), Reason> {
        let bytes = pattern.len();
        if bytes > MAX_SCHEMA_REGEX_SOURCE_BYTES {
            return Err(Reason::RegexTooLong {
                bytes,
                limit: MAX_SCHEMA_REGEX_SOURCE_BYTES,
            });
        }
        self.bytes += bytes;
        if self.bytes > MAX_SCHEMA_REGEX_SOURCE_BYTES_TOTAL {
            return Err(Reason::SchemaRegexesTooLong {
                bytes: self.bytes,
                limit: MAX_SCHEMA_REGEX_SOURCE_BYTES_TOTAL,
            });
        }

        // Podium's raw-text scans: an inline flag group first, then the
        // Unicode classes.
        if has_inline_flags(pattern) {
            return Err(invalid(pattern, INLINE_FLAGS));
        }
        let escapes = pattern.matches("\\p").count() + pattern.matches("\\P").count();
        if escapes > MAX_SCHEMA_REGEX_UNICODE_ESCAPES {
            return Err(Reason::RegexTooManyUnicodeEscapes {
                pattern: pattern.to_owned(),
                count: escapes,
                limit: MAX_SCHEMA_REGEX_UNICODE_ESCAPES,
            });
        }
        self.unicode_escapes += escapes;
        if self.unicode_escapes > MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL {
            return Err(Reason::SchemaRegexesTooManyUnicodeEscapes {
                count: self.unicode_escapes,
                limit: MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL,
            });
        }

        let size = instructions(pattern)?;
        if size > MAX_SCHEMA_REGEX_INSTRUCTIONS {
            return Err(Reason::RegexTooLarge {
                pattern: pattern.to_owned(),
                instructions: size,
                limit: MAX_SCHEMA_REGEX_INSTRUCTIONS,
            });
        }
        self.instructions = self.instructions.saturating_add(size);
        if self.instructions > MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL {
            return Err(Reason::SchemaRegexesTooLarge {
                instructions: self.instructions,
                limit: MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL,
            });
        }
        Ok(())
    }
}

/// Reports `(?` followed by a flag or `-` anywhere in the raw text.
fn has_inline_flags(pattern: &str) -> bool {
    pattern
        .as_bytes()
        .windows(3)
        .any(|w| w[0] == b'(' && w[1] == b'?' && matches!(w[2], b'i' | b'm' | b's' | b'U' | b'-'))
}

/// Parses a regex, checks its syntax, and measures Go's program for it as
/// Podium's `regexSize` does, stopped past [`MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL`].
pub(super) fn instructions(pattern: &str) -> Result<usize, Reason> {
    let ast = ast::parse::ParserBuilder::new()
        .nest_limit(NEST_LIMIT)
        .build()
        .parse(pattern)
        .map_err(|err| invalid(pattern, err.kind().to_string()))?;
    let mut parser = GoParser {
        pattern,
        stack: Vec::new(),
        captures: 0,
    };
    parser.emit(&ast)?;
    // The program's fail and match instructions.
    Ok(add(measure(&parser.finish()), 2))
}

fn invalid(pattern: &str, why: impl Into<String>) -> Reason {
    Reason::InvalidRegex {
        pattern: pattern.to_owned(),
        why: why.into(),
    }
}

/// The operators of Go's `regexp/syntax`, with Go's values: the order of the
/// character-class operators decides which one a merge keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Op {
    NoMatch = 1,
    EmptyMatch = 2,
    Literal = 3,
    CharClass = 4,
    AnyCharNotNl = 5,
    AnyChar = 6,
    BeginText = 9,
    EndText = 10,
    WordBoundary = 11,
    NoWordBoundary = 12,
    Capture = 13,
    Star = 14,
    Plus = 15,
    Quest = 16,
    Repeat = 17,
    Concat = 18,
    Alternate = 19,
    // The parse stack's pseudo-operators.
    LeftParen = 128,
    VerticalBar = 129,
}

/// A node of Go's parse tree, as `regexp/syntax` builds it. With no flag
/// groups, the only flags are case folding, for the literals Go makes of
/// classes like `[Aa]`, laziness, and the `$` of an end of text.
#[derive(Clone, Debug)]
struct Re {
    op: Op,
    fold: bool,
    lazy: bool,
    was_dollar: bool,
    /// A literal's runes, or a class's ranges as pairs: `[lo, hi, lo, hi, …]`.
    runes: Vec<u32>,
    min: u32,
    /// `None` for no upper bound.
    max: Option<u32>,
    /// A capture's index; 0 for a non-capturing group's parenthesis.
    cap: u32,
    name: Option<String>,
    subs: Vec<Re>,
}

impl Re {
    fn new(op: Op) -> Self {
        Self {
            op,
            fold: false,
            lazy: false,
            was_dollar: false,
            runes: Vec::new(),
            min: 0,
            max: None,
            cap: 0,
            name: None,
            subs: Vec::new(),
        }
    }

    fn literal(runes: Vec<u32>, fold: bool) -> Self {
        Self {
            runes,
            fold,
            ..Self::new(Op::Literal)
        }
    }

    fn class(runes: Vec<u32>) -> Self {
        Self {
            runes,
            ..Self::new(Op::CharClass)
        }
    }

    fn with_subs(op: Op, subs: Vec<Re>) -> Self {
        Self {
            subs,
            ..Self::new(op)
        }
    }
}

/// Takes a node out of a list, leaving an empty match in its place.
fn take(re: &mut Re) -> Re {
    std::mem::replace(re, Re::new(Op::EmptyMatch))
}

/// Replays Go's parser on a `regex-syntax` tree: the same pushes, merges and
/// reductions on the same stack, in the order Go's parser reads the source.
struct GoParser<'p> {
    pattern: &'p str,
    stack: Vec<Re>,
    captures: u32,
}

impl GoParser<'_> {
    fn invalid(&self, why: &str) -> Reason {
        invalid(self.pattern, why)
    }

    fn strict(&self, rule: SdkStrictRegexRule) -> Reason {
        Reason::SdkStrictRegex {
            pattern: self.pattern.to_owned(),
            rule,
        }
    }

    fn emit(&mut self, ast: &Ast) -> Result<(), Reason> {
        match ast {
            Ast::Empty(_) => Ok(()),
            // `(?x)`, `(?u)` and `(?R)`: the raw-text scan refuses the others.
            Ast::Flags(_) => Err(self.invalid(INLINE_FLAGS)),
            Ast::Literal(literal) => {
                let rune = self.literal_rune(literal)?;
                self.push(Re::literal(vec![rune], false));
                Ok(())
            }
            Ast::Dot(_) => {
                self.push(Re::new(Op::AnyCharNotNl));
                Ok(())
            }
            Ast::Assertion(assertion) => self.assertion(assertion),
            Ast::ClassUnicode(_) => Err(self.strict(SdkStrictRegexRule::UnicodeClass)),
            Ast::ClassPerl(class) => {
                let (table, _) = perl_table(&class.kind);
                let mut runes = Vec::new();
                append_group(&mut runes, table, class.negated);
                self.push(Re::class(runes));
                Ok(())
            }
            Ast::ClassBracketed(class) => {
                let runes = self.bracketed(class)?;
                self.push(Re::class(runes));
                Ok(())
            }
            Ast::Repetition(repetition) => self.repetition(repetition),
            Ast::Group(group) => self.group(group),
            Ast::Alternation(alternation) => {
                for (i, branch) in alternation.asts.iter().enumerate() {
                    if i > 0 {
                        self.vertical_bar();
                    }
                    self.emit(branch)?;
                }
                Ok(())
            }
            Ast::Concat(concat) => {
                for item in &concat.asts {
                    self.emit(item)?;
                }
                Ok(())
            }
        }
    }

    /// The rune of a literal outside brackets, if Go reads its escape alike.
    fn literal_rune(&self, literal: &ast::Literal) -> Result<u32, Reason> {
        use ast::{HexLiteralKind as Hex, LiteralKind as Kind, SpecialLiteralKind as Special};
        match literal.kind {
            Kind::Verbatim
            | Kind::Meta
            | Kind::Superfluous
            | Kind::HexFixed(Hex::X)
            | Kind::HexBrace(Hex::X)
            | Kind::Special(
                Special::Bell
                | Special::FormFeed
                | Special::Tab
                | Special::LineFeed
                | Special::CarriageReturn
                | Special::VerticalTab,
            ) => Ok(u32::from(literal.c)),
            Kind::HexFixed(Hex::UnicodeShort | Hex::UnicodeLong)
            | Kind::HexBrace(Hex::UnicodeShort | Hex::UnicodeLong) => {
                Err(self.invalid("\\u and \\U escapes are not RE2 syntax: write \\x{…}"))
            }
            // Octal escapes are off in the parser, and `\ ` needs the x flag.
            Kind::Octal | Kind::Special(Special::Space) => {
                Err(self.invalid("this escape is not RE2 syntax"))
            }
        }
    }

    fn assertion(&mut self, assertion: &ast::Assertion) -> Result<(), Reason> {
        use ast::AssertionKind as Kind;
        // With no flags, `^` and `$` are the beginning and end of the text.
        let re = match assertion.kind {
            Kind::StartLine | Kind::StartText => Re::new(Op::BeginText),
            Kind::EndLine => Re {
                was_dollar: true,
                ..Re::new(Op::EndText)
            },
            Kind::EndText => Re::new(Op::EndText),
            Kind::WordBoundary => Re::new(Op::WordBoundary),
            Kind::NotWordBoundary => Re::new(Op::NoWordBoundary),
            Kind::WordBoundaryStart
            | Kind::WordBoundaryEnd
            | Kind::WordBoundaryStartAngle
            | Kind::WordBoundaryEndAngle
            | Kind::WordBoundaryStartHalf
            | Kind::WordBoundaryEndHalf => {
                return Err(self.strict(SdkStrictRegexRule::WordBoundaryExtension));
            }
        };
        self.push(re);
        Ok(())
    }

    fn group(&mut self, group: &ast::Group) -> Result<(), Reason> {
        let (cap, name) = match &group.kind {
            ast::GroupKind::CaptureIndex(_) => (self.next_capture(), None),
            ast::GroupKind::CaptureName { name, .. } => {
                let go_name = name
                    .name
                    .bytes()
                    .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
                if !go_name {
                    return Err(self.invalid(
                        "a capture name may hold only ASCII letters, digits and `_` in RE2",
                    ));
                }
                (self.next_capture(), Some(name.name.clone()))
            }
            ast::GroupKind::NonCapturing(flags) => {
                if !flags.items.is_empty() {
                    return Err(self.invalid(INLINE_FLAGS));
                }
                (0, None)
            }
        };
        self.push(Re {
            cap,
            name,
            ..Re::new(Op::LeftParen)
        });
        self.emit(&group.ast)?;
        self.right_paren();
        Ok(())
    }

    fn next_capture(&mut self) -> u32 {
        self.captures += 1;
        self.captures
    }

    fn repetition(&mut self, repetition: &ast::Repetition) -> Result<(), Reason> {
        use ast::{RepetitionKind as Kind, RepetitionRange as Range};
        let (op, min, max) = match &repetition.op.kind {
            Kind::ZeroOrOne => (Op::Quest, 0, Some(0)),
            Kind::ZeroOrMore => (Op::Star, 0, Some(0)),
            Kind::OneOrMore => (Op::Plus, 0, Some(0)),
            Kind::Range(range) => {
                if !self.reads_as_go_repeat(repetition) {
                    return Err(self.strict(SdkStrictRegexRule::RepetitionReadAsLiteral));
                }
                let (min, max) = match *range {
                    Range::Exactly(n) => (n, Some(n)),
                    Range::AtLeast(n) => (n, None),
                    Range::Bounded(m, n) => (m, Some(n)),
                };
                (Op::Repeat, min, max)
            }
        };
        if let Ast::Repetition(inner) = &*repetition.ast {
            if matches!(inner.op.kind, Kind::Range(_)) && !self.reads_as_go_repeat(inner) {
                return Err(self.strict(SdkStrictRegexRule::RepetitionReadAsLiteral));
            }
            return Err(self.invalid(
                "a repetition operator follows another, which RE2 refuses: write (?:x*)* for x**",
            ));
        }
        if op == Op::Repeat && (min > MAX_REPEAT || max.is_some_and(|max| max > MAX_REPEAT)) {
            return Err(self.invalid("RE2 refuses a repetition count over 1 000"));
        }
        self.emit(&repetition.ast)?;
        self.repeat(op, min, max, !repetition.greedy)
    }

    /// Reports a counted repetition whose source Go's `parseRepeat` reads as
    /// one: `{n}`, `{n,}` or `{n,m}`, with no spaces and no leading zeros.
    fn reads_as_go_repeat(&self, repetition: &ast::Repetition) -> bool {
        let span = &repetition.op.span;
        let mut text = &self.pattern[span.start.offset..span.end.offset];
        if !repetition.greedy {
            let Some(greedy) = text.strip_suffix('?') else {
                return false;
            };
            text = greedy;
        }
        let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) else {
            return false;
        };
        let int = |s: &str| {
            !s.is_empty()
                && s.bytes().all(|byte| byte.is_ascii_digit())
                && !(s.len() > 1 && s.starts_with('0'))
        };
        match inner.split_once(',') {
            None => int(inner),
            Some((min, max)) => int(min) && (max.is_empty() || int(max)),
        }
    }

    /// The ranges of a bracketed class, as Go's `parseClass` reads it, if Go
    /// reads it as `regex-syntax` does.
    fn bracketed(&self, class: &ast::ClassBracketed) -> Result<Vec<u32>, Reason> {
        if set_has_unicode(&class.kind) {
            return Err(self.strict(SdkStrictRegexRule::UnicodeClass));
        }
        let Some(go) = go_class(self.pattern, class.span.start.offset) else {
            return Err(self.invalid("Go's RE2 parser refuses this bracketed class"));
        };
        if set_has_operation(&class.kind) {
            return Err(self.strict(SdkStrictRegexRule::ClassSetOperation));
        }
        let mut items = Vec::new();
        let flat = match &class.kind {
            ast::ClassSet::Item(item) => flatten(item, &mut items),
            ast::ClassSet::BinaryOp(_) => false,
        };
        if !flat
            || go.end != class.span.end.offset
            || go.negated != class.negated
            || go.items != items
        {
            return Err(self.strict(SdkStrictRegexRule::ClassReadDifferently));
        }
        Ok(go.runes)
    }

    // Go's parse-stack operations (`regexp/syntax/parse.go`).

    /// Go's `push`: a one-rune class becomes a literal, and so does a class
    /// of two runes that fold to each other, case-folded; a literal pushed
    /// after two literals of the same folding merges them first.
    fn push(&mut self, mut re: Re) {
        if re.op == Op::CharClass && re.runes.len() == 2 && re.runes[0] == re.runes[1] {
            if self.maybe_concat(Some((re.runes[0], false))) {
                return;
            }
            re.op = Op::Literal;
            re.runes.truncate(1);
            re.fold = false;
        } else if re.op == Op::CharClass && is_fold_pair(&re.runes) {
            if self.maybe_concat(Some((re.runes[0], true))) {
                return;
            }
            re.op = Op::Literal;
            re.runes.truncate(1);
            re.fold = true;
        } else {
            self.maybe_concat(None);
        }
        self.stack.push(re);
    }

    /// Go's `maybeConcat`: merges the top two literals of the stack if they
    /// fold alike, then pushes `rune` in the second one's place.
    fn maybe_concat(&mut self, rune: Option<(u32, bool)>) -> bool {
        let n = self.stack.len();
        if n < 2 {
            return false;
        }
        let (re2, re1) = (&self.stack[n - 2], &self.stack[n - 1]);
        if re1.op != Op::Literal || re2.op != Op::Literal || re1.fold != re2.fold {
            return false;
        }
        let Some(re1) = self.stack.pop() else {
            return false;
        };
        self.stack[n - 2].runes.extend(re1.runes);
        match rune {
            Some((rune, fold)) => {
                self.stack.push(Re::literal(vec![rune], fold));
                true
            }
            None => false,
        }
    }

    /// Go's `repeat`: replaces the top of the stack with itself repeated.
    fn repeat(&mut self, op: Op, min: u32, max: Option<u32>, lazy: bool) -> Result<(), Reason> {
        // regex-syntax refuses a repetition with nothing to repeat.
        let Some(sub) = self.stack.pop() else {
            return Err(self.invalid("missing argument to repetition operator"));
        };
        let re = Re {
            min,
            max,
            lazy,
            subs: vec![sub],
            ..Re::new(op)
        };
        if op == Op::Repeat
            && (min >= 2 || max.is_some_and(|max| max >= 2))
            && !repeat_is_valid(&re, MAX_REPEAT)
        {
            return Err(self.invalid(
                "nested counted repetitions make more than 1 000 copies, which RE2 refuses",
            ));
        }
        self.stack.push(re);
        Ok(())
    }

    /// The stack's elements above its top pseudo-operator start here.
    fn operands_start(&self) -> usize {
        self.stack
            .iter()
            .rposition(|re| re.op >= Op::LeftParen)
            .map_or(0, |i| i + 1)
    }

    /// Go's `concat`.
    fn concat(&mut self) {
        self.maybe_concat(None);
        let subs = self.stack.split_off(self.operands_start());
        if subs.is_empty() {
            self.push(Re::new(Op::EmptyMatch));
            return;
        }
        let re = collapse(subs, Op::Concat);
        self.push(re);
    }

    /// Go's `alternate`.
    fn alternate(&mut self) {
        let mut subs = self.stack.split_off(self.operands_start());
        let Some(last) = subs.last_mut() else {
            self.push(Re::new(Op::NoMatch));
            return;
        };
        clean_alt(last);
        let re = collapse(subs, Op::Alternate);
        self.push(re);
    }

    /// Go's `parseVerticalBar`.
    fn vertical_bar(&mut self) {
        self.concat();
        if !self.swap_vertical_bar() {
            self.push(Re::new(Op::VerticalBar));
        }
    }

    /// Go's `swapVerticalBar`: moves the branch just parsed below the bar,
    /// merging it into the branch before if both are character classes.
    fn swap_vertical_bar(&mut self) -> bool {
        let n = self.stack.len();
        if n >= 3
            && self.stack[n - 2].op == Op::VerticalBar
            && is_char_class(&self.stack[n - 1])
            && is_char_class(&self.stack[n - 3])
        {
            let Some(mut re1) = self.stack.pop() else {
                return false;
            };
            let re3 = &mut self.stack[n - 3];
            if re1.op > re3.op {
                std::mem::swap(re3, &mut re1);
            }
            merge_char_class(re3, &re1);
            return true;
        }
        if n >= 2 && self.stack[n - 2].op == Op::VerticalBar {
            if n >= 3 {
                clean_alt(&mut self.stack[n - 3]);
            }
            self.stack.swap(n - 2, n - 1);
            return true;
        }
        false
    }

    /// Go's `parseRightParen`.
    fn right_paren(&mut self) {
        self.concat();
        if self.swap_vertical_bar() {
            self.stack.pop();
        }
        self.alternate();
        // regex-syntax balances parentheses.
        let (Some(re1), Some(re2)) = (self.stack.pop(), self.stack.pop()) else {
            return;
        };
        if re2.cap == 0 {
            self.push(re1);
        } else {
            self.push(Re {
                cap: re2.cap,
                name: re2.name,
                subs: vec![re1],
                ..Re::new(Op::Capture)
            });
        }
    }

    /// The end of Go's `parse`: the tree left on the stack.
    fn finish(mut self) -> Re {
        self.concat();
        if self.swap_vertical_bar() {
            self.stack.pop();
        }
        self.alternate();
        self.stack.pop().unwrap_or_else(|| Re::new(Op::EmptyMatch))
    }
}

/// Go's `repeatIsValid`: the nested counts multiply to at most `n` copies of
/// the innermost thing.
fn repeat_is_valid(re: &Re, mut n: u32) -> bool {
    if re.op == Op::Repeat {
        let m = match re.max {
            Some(0) => return true,
            Some(max) => max,
            None => re.min,
        };
        if m > n {
            return false;
        }
        if m > 0 {
            n /= m;
        }
    }
    re.subs.iter().all(|sub| repeat_is_valid(sub, n))
}

/// Go's `collapse`: applies `op` to `subs`, hoisting nested nodes of the same
/// operator, and factors an alternation.
fn collapse(mut subs: Vec<Re>, op: Op) -> Re {
    if subs.len() == 1
        && let Some(only) = subs.pop()
    {
        return only;
    }
    let mut flat = Vec::with_capacity(subs.len());
    for sub in subs {
        if sub.op == op {
            flat.extend(sub.subs);
        } else {
            flat.push(sub);
        }
    }
    if op != Op::Alternate {
        return Re::with_subs(op, flat);
    }
    let mut factored = factor(flat);
    if factored.len() == 1
        && let Some(only) = factored.pop()
    {
        return only;
    }
    Re::with_subs(op, factored)
}

/// Go's `factor`: the four rounds that simplify the branches of an alternation.
fn factor(mut sub: Vec<Re>) -> Vec<Re> {
    if sub.len() < 2 {
        return sub;
    }

    // Round 1: factor out common literal prefixes.
    let mut out = Vec::with_capacity(sub.len());
    let mut start = 0;
    let mut prefix: Vec<u32> = Vec::new();
    let mut prefix_fold = false;
    for i in 0..=sub.len() {
        let mut next: Vec<u32> = Vec::new();
        let mut next_fold = false;
        if i < sub.len() {
            let (runes, fold) = leading_string(&sub[i]);
            next = runes.to_vec();
            next_fold = fold;
            if next_fold == prefix_fold {
                let same = prefix.iter().zip(&next).take_while(|(a, b)| a == b).count();
                if same > 0 {
                    prefix.truncate(same);
                    continue;
                }
            }
        }
        if i == start + 1 {
            out.push(take(&mut sub[start]));
        } else if i > start + 1 {
            let literal = Re::literal(prefix.clone(), prefix_fold);
            let suffixes = (start..i)
                .map(|j| remove_leading_string(take(&mut sub[j]), prefix.len()))
                .collect();
            let suffix = collapse(suffixes, Op::Alternate);
            out.push(Re::with_subs(Op::Concat, vec![literal, suffix]));
        }
        start = i;
        prefix = next;
        prefix_fold = next_fold;
    }
    let mut sub = out;

    // Round 2: factor out common simple prefixes, the first piece of each
    // concatenation, when it is a class or a fixed repeat of one.
    let mut out = Vec::with_capacity(sub.len());
    let mut start = 0;
    for i in 0..=sub.len() {
        if i < sub.len()
            && start < i
            && let (Some(first), Some(next)) =
                (leading_regexp(&sub[start]), leading_regexp(&sub[i]))
        {
            let simple = is_char_class(first)
                || (first.op == Op::Repeat
                    && Some(first.min) == first.max
                    && first.subs.first().is_some_and(is_char_class));
            if simple && equal(first, next) {
                continue;
            }
        }
        if i == start + 1 {
            out.push(take(&mut sub[start]));
        } else if i > start + 1
            && let Some(first) = leading_regexp(&sub[start]).cloned()
        {
            let suffixes = (start..i)
                .map(|j| remove_leading_regexp(take(&mut sub[j])))
                .collect();
            let suffix = collapse(suffixes, Op::Alternate);
            out.push(Re::with_subs(Op::Concat, vec![first, suffix]));
        }
        start = i;
    }
    let mut sub = out;

    // Round 3: collapse runs of single literals into character classes.
    let mut out = Vec::with_capacity(sub.len());
    let mut start = 0;
    for i in 0..=sub.len() {
        if i < sub.len() && is_char_class(&sub[i]) {
            continue;
        }
        if i == start + 1 {
            out.push(take(&mut sub[start]));
        } else if i > start + 1 {
            // Start with the most complex one.
            let mut max = start;
            for j in start + 1..i {
                if sub[max].op < sub[j].op
                    || (sub[max].op == sub[j].op && sub[max].runes.len() < sub[j].runes.len())
                {
                    max = j;
                }
            }
            sub.swap(start, max);
            let mut class = take(&mut sub[start]);
            for other in &sub[start + 1..i] {
                merge_char_class(&mut class, other);
            }
            clean_alt(&mut class);
            out.push(class);
        }
        if i < sub.len() {
            out.push(take(&mut sub[i]));
        }
        start = i + 1;
    }
    let sub = out;

    // Round 4: collapse runs of empty matches into one.
    let ops: Vec<Op> = sub.iter().map(|re| re.op).collect();
    sub.into_iter()
        .enumerate()
        .filter(|(i, _)| !(ops[*i] == Op::EmptyMatch && ops.get(i + 1) == Some(&Op::EmptyMatch)))
        .map(|(_, re)| re)
        .collect()
}

/// Go's `leadingString`.
fn leading_string(re: &Re) -> (&[u32], bool) {
    let re = match re.subs.first() {
        Some(first) if re.op == Op::Concat => first,
        _ => re,
    };
    if re.op == Op::Literal {
        (&re.runes, re.fold)
    } else {
        (&[], false)
    }
}

/// Go's `removeLeadingString`.
fn remove_leading_string(mut re: Re, n: usize) -> Re {
    if re.op == Op::Concat && !re.subs.is_empty() {
        let first = remove_leading_string(take(&mut re.subs[0]), n);
        let emptied = first.op == Op::EmptyMatch;
        re.subs[0] = first;
        if emptied {
            match re.subs.len() {
                0 | 1 => {
                    re.op = Op::EmptyMatch;
                    re.subs.clear();
                }
                2 => {
                    if let Some(second) = re.subs.pop() {
                        return second;
                    }
                }
                _ => {
                    re.subs.remove(0);
                }
            }
        }
        return re;
    }
    if re.op == Op::Literal {
        re.runes.drain(..n.min(re.runes.len()));
        if re.runes.is_empty() {
            re.op = Op::EmptyMatch;
        }
    }
    re
}

/// Go's `leadingRegexp`.
fn leading_regexp(re: &Re) -> Option<&Re> {
    if re.op == Op::EmptyMatch {
        return None;
    }
    if re.op == Op::Concat
        && let Some(first) = re.subs.first()
    {
        return (first.op != Op::EmptyMatch).then_some(first);
    }
    Some(re)
}

/// Go's `removeLeadingRegexp`.
fn remove_leading_regexp(mut re: Re) -> Re {
    if re.op == Op::Concat && !re.subs.is_empty() {
        re.subs.remove(0);
        match re.subs.len() {
            0 => re.op = Op::EmptyMatch,
            1 => {
                if let Some(only) = re.subs.pop() {
                    return only;
                }
            }
            _ => {}
        }
        return re;
    }
    Re::new(Op::EmptyMatch)
}

/// Go's `Regexp.Equal` (Go 1.27).
fn equal(x: &Re, y: &Re) -> bool {
    if x.op != y.op {
        return false;
    }
    let subs =
        || x.subs.len() == y.subs.len() && x.subs.iter().zip(&y.subs).all(|(a, b)| equal(a, b));
    match x.op {
        Op::EndText => x.was_dollar == y.was_dollar,
        Op::Literal | Op::CharClass => x.fold == y.fold && x.runes == y.runes,
        Op::Alternate | Op::Concat => subs(),
        Op::Star | Op::Plus | Op::Quest => x.lazy == y.lazy && subs(),
        Op::Repeat => x.lazy == y.lazy && x.min == y.min && x.max == y.max && subs(),
        Op::Capture => x.cap == y.cap && x.name == y.name && subs(),
        _ => true,
    }
}

/// Go's `isCharClass`: a single-rune literal, a class, or a dot.
fn is_char_class(re: &Re) -> bool {
    (re.op == Op::Literal && re.runes.len() == 1)
        || matches!(re.op, Op::CharClass | Op::AnyCharNotNl | Op::AnyChar)
}

/// Go's `matchRune`.
fn match_rune(re: &Re, rune: u32) -> bool {
    match re.op {
        Op::Literal => re.runes.len() == 1 && re.runes[0] == rune,
        Op::CharClass => re
            .runes
            .chunks_exact(2)
            .any(|pair| (pair[0]..=pair[1]).contains(&rune)),
        Op::AnyCharNotNl => rune != u32::from('\n'),
        Op::AnyChar => true,
        _ => false,
    }
}

/// Go's `mergeCharClass`: `dst = dst|src`, where `dst` is the more complex.
fn merge_char_class(dst: &mut Re, src: &Re) {
    match dst.op {
        Op::AnyCharNotNl => {
            if match_rune(src, u32::from('\n')) {
                dst.op = Op::AnyChar;
            }
        }
        Op::CharClass => {
            if src.op == Op::Literal {
                append_literal(&mut dst.runes, src.runes[0], src.fold);
            } else {
                append_class(&mut dst.runes, &src.runes);
            }
        }
        Op::Literal => {
            if src.runes[0] == dst.runes[0] && src.fold == dst.fold {
                return;
            }
            let first = dst.runes[0];
            dst.op = Op::CharClass;
            dst.runes.clear();
            append_literal(&mut dst.runes, first, dst.fold);
            append_literal(&mut dst.runes, src.runes[0], src.fold);
        }
        _ => {}
    }
}

/// Go's `cleanAlt`: cleans a class for an alternation, and makes a dot of a
/// class of every rune, or of every rune but `\n`.
fn clean_alt(re: &mut Re) {
    if re.op != Op::CharClass {
        return;
    }
    clean_class(&mut re.runes);
    if re.runes == [0, MAX_RUNE] {
        re.runes.clear();
        re.op = Op::AnyChar;
    } else if re.runes == [0, 0x09, 0x0B, MAX_RUNE] {
        re.runes.clear();
        re.op = Op::AnyCharNotNl;
    }
}

/// Podium's `measureRegex` with each class counted by its ranges.
fn measure(re: &Re) -> usize {
    let sub = || re.subs.first().map_or(1, measure);
    match re.op {
        Op::Literal => re.runes.len().max(1),
        Op::CharClass => (re.runes.len() / 2).max(1),
        // A star is a loop, or (x+)? when x matches the empty string.
        Op::Capture | Op::Star => add(sub(), 2),
        Op::Plus | Op::Quest => add(sub(), 1),
        Op::Repeat => {
            let sub = sub();
            let min = re.min as usize;
            match re.max {
                None if re.min <= 1 => add(sub, 2),
                // n-1 copies of x, then x+.
                None => add(mul(min, sub), 1),
                Some(0) => 1,
                Some(max) => add(mul(min, sub), mul(max as usize - min, add(sub, 1))),
            }
        }
        // The alternation's branches, or the concatenation's no-op.
        Op::Concat | Op::Alternate => re
            .subs
            .iter()
            .fold(re.subs.len(), |total, sub| add(total, measure(sub))),
        // Any character, an empty-width assertion, or an empty or failing match.
        _ => 1,
    }
}

fn add(a: usize, b: usize) -> usize {
    a.saturating_add(b).min(SATURATED)
}

fn mul(n: usize, size: usize) -> usize {
    if n == 0 || size == 0 {
        0
    } else if n > SATURATED / size {
        SATURATED
    } else {
        n * size
    }
}

// Character classes, as Go builds them: flat lists of range pairs.

/// Go's `\d`, `\s` and `\w` (`perl_groups.go`).
const PERL_DIGIT: &[u32] = &[0x30, 0x39];
const PERL_SPACE: &[u32] = &[0x09, 0x0A, 0x0C, 0x0D, 0x20, 0x20];
const PERL_WORD: &[u32] = &[0x30, 0x39, 0x41, 0x5A, 0x5F, 0x5F, 0x61, 0x7A];

/// Go's POSIX classes (`perl_groups.go`), by the names `regex-syntax` gives them.
const POSIX: [(&str, &[u32]); 14] = [
    ("alnum", &[0x30, 0x39, 0x41, 0x5A, 0x61, 0x7A]),
    ("alpha", &[0x41, 0x5A, 0x61, 0x7A]),
    ("ascii", &[0x00, 0x7F]),
    ("blank", &[0x09, 0x09, 0x20, 0x20]),
    ("cntrl", &[0x00, 0x1F, 0x7F, 0x7F]),
    ("digit", &[0x30, 0x39]),
    ("graph", &[0x21, 0x7E]),
    ("lower", &[0x61, 0x7A]),
    ("print", &[0x20, 0x7E]),
    ("punct", &[0x21, 0x2F, 0x3A, 0x40, 0x5B, 0x60, 0x7B, 0x7E]),
    ("space", &[0x09, 0x0D, 0x20, 0x20]),
    ("upper", &[0x41, 0x5A]),
    ("word", &[0x30, 0x39, 0x41, 0x5A, 0x5F, 0x5F, 0x61, 0x7A]),
    ("xdigit", &[0x30, 0x39, 0x41, 0x46, 0x61, 0x66]),
];

/// An item of a bracketed class, by which Go's reading and `regex-syntax`'s
/// are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClassItem {
    Range(u32, u32),
    Posix(&'static str, bool),
    Perl(u8, bool),
}

/// What Go's `parseClass` reads from the class that starts at `[`.
struct GoClass {
    negated: bool,
    items: Vec<ClassItem>,
    /// The offset in the pattern just past the class.
    end: usize,
    runes: Vec<u32>,
}

fn perl_table(kind: &ast::ClassPerlKind) -> (&'static [u32], u8) {
    match kind {
        ast::ClassPerlKind::Digit => (PERL_DIGIT, b'd'),
        ast::ClassPerlKind::Space => (PERL_SPACE, b's'),
        ast::ClassPerlKind::Word => (PERL_WORD, b'w'),
    }
}

fn ascii_name(kind: &ast::ClassAsciiKind) -> &'static str {
    use ast::ClassAsciiKind as Kind;
    match kind {
        Kind::Alnum => "alnum",
        Kind::Alpha => "alpha",
        Kind::Ascii => "ascii",
        Kind::Blank => "blank",
        Kind::Cntrl => "cntrl",
        Kind::Digit => "digit",
        Kind::Graph => "graph",
        Kind::Lower => "lower",
        Kind::Print => "print",
        Kind::Punct => "punct",
        Kind::Space => "space",
        Kind::Upper => "upper",
        Kind::Word => "word",
        Kind::Xdigit => "xdigit",
    }
}

/// Go's `parseClass` on the pattern from the `[` at `start`: `None` where Go
/// refuses the class, or where it would read a Unicode class, which the caller
/// has refused already.
fn go_class(pattern: &str, start: usize) -> Option<GoClass> {
    let mut t = pattern.get(start + 1..)?;
    let mut negated = false;
    if let Some(rest) = t.strip_prefix('^') {
        negated = true;
        t = rest;
    }
    let mut runes = Vec::new();
    let mut items = Vec::new();
    // `]` is a literal as the first character.
    let mut first = true;
    while t.is_empty() || !t.starts_with(']') || first {
        first = false;
        // A POSIX class runs to the first `:]` of the rest of the pattern.
        if t.len() > 2
            && t.starts_with("[:")
            && let Some(found) = t[2..].find(":]")
        {
            let end = 2 + found + 2;
            let inner = &t[2..end - 2];
            let (name, group_negated) = match inner.strip_prefix('^') {
                Some(name) => (name, true),
                None => (inner, false),
            };
            let &(name, table) = POSIX.iter().find(|entry| entry.0 == name)?;
            append_group(&mut runes, table, group_negated);
            items.push(ClassItem::Posix(name, group_negated));
            t = &t[end..];
            continue;
        }
        let bytes = t.as_bytes();
        if bytes.len() >= 2 && bytes[0] == b'\\' {
            if matches!(bytes[1], b'p' | b'P') {
                return None;
            }
            let perl = match bytes[1] {
                b'd' | b'D' => Some((PERL_DIGIT, b'd')),
                b's' | b'S' => Some((PERL_SPACE, b's')),
                b'w' | b'W' => Some((PERL_WORD, b'w')),
                _ => None,
            };
            if let Some((table, letter)) = perl {
                let group_negated = bytes[1].is_ascii_uppercase();
                append_group(&mut runes, table, group_negated);
                items.push(ClassItem::Perl(letter, group_negated));
                t = &t[2..];
                continue;
            }
        }
        let (lo, rest) = go_class_char(t)?;
        t = rest;
        let mut hi = lo;
        // `[a-]` is `a` or `-`.
        if t.len() >= 2 && t.starts_with('-') && !t[1..].starts_with(']') {
            let (end, rest) = go_class_char(&t[1..])?;
            if end < lo {
                return None;
            }
            hi = end;
            t = rest;
        }
        append_range(&mut runes, lo, hi);
        items.push(ClassItem::Range(lo, hi));
    }
    t = &t[1..];
    clean_class(&mut runes);
    if negated {
        runes = negate_class(&runes);
    }
    Some(GoClass {
        negated,
        items,
        end: pattern.len() - t.len(),
        runes,
    })
}

/// Go's `parseClassChar`.
fn go_class_char(t: &str) -> Option<(u32, &str)> {
    if t.starts_with('\\') {
        return go_escape(t);
    }
    let c = t.chars().next()?;
    Some((u32::from(c), &t[c.len_utf8()..]))
}

/// Go's `parseEscape`, on text that starts with a backslash.
fn go_escape(s: &str) -> Option<(u32, &str)> {
    let mut chars = s.get(1..)?.chars();
    let c = chars.next()?;
    let t = chars.as_str();
    let octal = |byte: Option<&u8>| byte.is_some_and(|byte| (b'0'..=b'7').contains(byte));
    match c {
        // A single non-zero digit is a back-reference.
        '1'..='7' if !octal(t.as_bytes().first()) => None,
        '0'..='7' => {
            let mut rune = u32::from(c) - u32::from('0');
            let mut t = t;
            for _ in 1..3 {
                match t.as_bytes().first() {
                    Some(&digit @ b'0'..=b'7') => {
                        rune = rune * 8 + u32::from(digit - b'0');
                        t = &t[1..];
                    }
                    _ => break,
                }
            }
            Some((rune, t))
        }
        'x' => {
            let mut chars = t.chars();
            let first = chars.next()?;
            if first == '{' {
                let mut rune = 0_u32;
                let mut digits = 0;
                loop {
                    let c = chars.next()?;
                    if c == '}' {
                        break;
                    }
                    rune = rune * 16 + c.to_digit(16)?;
                    if rune > MAX_RUNE {
                        return None;
                    }
                    digits += 1;
                }
                return (digits > 0).then_some((rune, chars.as_str()));
            }
            let high = first.to_digit(16)?;
            let low = chars.next()?.to_digit(16)?;
            Some((high * 16 + low, chars.as_str()))
        }
        'a' => Some((0x07, t)),
        'f' => Some((0x0C, t)),
        'n' => Some((0x0A, t)),
        'r' => Some((0x0D, t)),
        't' => Some((0x09, t)),
        'v' => Some((0x0B, t)),
        // Escaped punctuation is always itself.
        c if c.is_ascii() && !c.is_ascii_alphanumeric() => Some((u32::from(c), t)),
        _ => None,
    }
}

fn set_has_unicode(set: &ast::ClassSet) -> bool {
    match set {
        ast::ClassSet::Item(item) => item_has_unicode(item),
        ast::ClassSet::BinaryOp(op) => set_has_unicode(&op.lhs) || set_has_unicode(&op.rhs),
    }
}

fn item_has_unicode(item: &ast::ClassSetItem) -> bool {
    match item {
        ast::ClassSetItem::Unicode(_) => true,
        ast::ClassSetItem::Bracketed(class) => set_has_unicode(&class.kind),
        ast::ClassSetItem::Union(union) => union.items.iter().any(item_has_unicode),
        _ => false,
    }
}

fn set_has_operation(set: &ast::ClassSet) -> bool {
    match set {
        ast::ClassSet::Item(item) => item_has_operation(item),
        ast::ClassSet::BinaryOp(_) => true,
    }
}

fn item_has_operation(item: &ast::ClassSetItem) -> bool {
    match item {
        ast::ClassSetItem::Bracketed(class) => set_has_operation(&class.kind),
        ast::ClassSetItem::Union(union) => union.items.iter().any(item_has_operation),
        _ => false,
    }
}

/// The items of a class as `regex-syntax` reads them; `false` for a nested
/// class, which Go reads otherwise.
fn flatten(item: &ast::ClassSetItem, items: &mut Vec<ClassItem>) -> bool {
    match item {
        ast::ClassSetItem::Empty(_) => true,
        ast::ClassSetItem::Literal(literal) => {
            items.push(ClassItem::Range(u32::from(literal.c), u32::from(literal.c)));
            true
        }
        ast::ClassSetItem::Range(range) => {
            items.push(ClassItem::Range(
                u32::from(range.start.c),
                u32::from(range.end.c),
            ));
            true
        }
        ast::ClassSetItem::Ascii(class) => {
            items.push(ClassItem::Posix(ascii_name(&class.kind), class.negated));
            true
        }
        ast::ClassSetItem::Perl(class) => {
            items.push(ClassItem::Perl(perl_table(&class.kind).1, class.negated));
            true
        }
        ast::ClassSetItem::Unicode(_) | ast::ClassSetItem::Bracketed(_) => false,
        ast::ClassSetItem::Union(union) => union.items.iter().all(|item| flatten(item, items)),
    }
}

/// Go's `appendGroup`, with no case folding.
fn append_group(runes: &mut Vec<u32>, table: &[u32], negated: bool) {
    if negated {
        append_negated_class(runes, table);
    } else {
        append_class(runes, table);
    }
}

/// Go's `appendRange`: extends the last or the second-to-last range when the
/// new one overlaps or abuts it.
fn append_range(runes: &mut Vec<u32>, lo: u32, hi: u32) {
    let n = runes.len();
    for i in [2, 4] {
        if n >= i {
            let (rlo, rhi) = (runes[n - i], runes[n - i + 1]);
            if lo <= rhi + 1 && rlo <= hi + 1 {
                runes[n - i] = rlo.min(lo);
                runes[n - i + 1] = rhi.max(hi);
                return;
            }
        }
    }
    runes.extend([lo, hi]);
}

fn append_class(runes: &mut Vec<u32>, class: &[u32]) {
    for pair in class.chunks_exact(2) {
        append_range(runes, pair[0], pair[1]);
    }
}

/// Go's `appendNegatedClass`, for a clean `class`.
fn append_negated_class(runes: &mut Vec<u32>, class: &[u32]) {
    let mut next = 0;
    for pair in class.chunks_exact(2) {
        if pair[0] > next {
            append_range(runes, next, pair[0] - 1);
        }
        next = pair[1] + 1;
    }
    if next <= MAX_RUNE {
        append_range(runes, next, MAX_RUNE);
    }
}

/// Go's `appendLiteral`.
fn append_literal(runes: &mut Vec<u32>, rune: u32, fold: bool) {
    if fold {
        append_folded_rune(runes, rune);
    } else {
        append_range(runes, rune, rune);
    }
}

/// Go's `appendFoldedRange` for one rune: the rune and its case-folding orbit.
/// Folded literals come only from classes like `[Aa]`, whose orbits are ASCII.
fn append_folded_rune(runes: &mut Vec<u32>, rune: u32) {
    append_range(runes, rune, rune);
    let mut folded = simple_fold(rune).unwrap_or(rune);
    while folded != rune {
        append_range(runes, folded, folded);
        folded = simple_fold(folded).unwrap_or(rune);
    }
}

/// Go's `cleanClass`: sorts the ranges and merges those that overlap or abut.
fn clean_class(runes: &mut Vec<u32>) {
    let mut pairs: Vec<(u32, u32)> = runes
        .chunks_exact(2)
        .map(|pair| (pair[0], pair[1]))
        .collect();
    pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    runes.clear();
    for (lo, hi) in pairs {
        if let Some(last) = runes.last_mut()
            && lo <= *last + 1
        {
            *last = (*last).max(hi);
            continue;
        }
        runes.extend([lo, hi]);
    }
}

/// Go's `negateClass`, for a clean class.
fn negate_class(runes: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(runes.len() + 2);
    let mut next = 0;
    for pair in runes.chunks_exact(2) {
        if pair[0] > next {
            out.extend([next, pair[0] - 1]);
        }
        next = pair[1] + 1;
    }
    if next <= MAX_RUNE {
        out.extend([next, MAX_RUNE]);
    }
    out
}

/// Go's `unicode.SimpleFold` for the runes whose case-folding orbit holds an
/// ASCII letter; `None` for the other non-ASCII runes, whose folding Go reads
/// from its Unicode tables.
fn simple_fold(rune: u32) -> Option<u32> {
    match rune {
        // K, k and the Kelvin sign; S, s and the long s.
        0x4B => Some(0x6B),
        0x6B => Some(0x212A),
        0x212A => Some(0x4B),
        0x53 => Some(0x73),
        0x73 => Some(0x017F),
        0x017F => Some(0x53),
        0x41..=0x5A => Some(rune + 0x20),
        0x61..=0x7A => Some(rune - 0x20),
        0x00..=0x7F => Some(rune),
        _ => None,
    }
}

/// Reports the ranges of a class of two runes that fold to each other, which
/// Go rewrites as a case-folded literal: `[Aa]`, or two adjacent runes.
fn is_fold_pair(runes: &[u32]) -> bool {
    let folds = |a: u32, b: u32| simple_fold(a) == Some(b) && simple_fold(b) == Some(a);
    match *runes {
        [a, a2, b, b2] => a == a2 && b == b2 && folds(a, b),
        [a, b] => a + 1 == b && folds(a, b),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::{Map, Value, json};

    use super::instructions;
    use crate::{
        MAX_SCHEMA_REGEX_INSTRUCTIONS, MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL,
        MAX_SCHEMA_REGEX_SOURCE_BYTES, MAX_SCHEMA_REGEX_SOURCE_BYTES_TOTAL,
        MAX_SCHEMA_REGEX_UNICODE_ESCAPES, MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL,
        SdkStrictRegexRule as Rule, WorkloadSchema, WorkloadSchemaError,
        WorkloadSchemaErrorReason as Reason,
    };

    fn verdict(pattern: &str) -> Result<WorkloadSchema, WorkloadSchemaError> {
        WorkloadSchema::from_value(&json!({ "pattern": pattern }))
    }

    fn accepted(pattern: &str) {
        if let Err(err) = verdict(pattern) {
            panic!("{pattern:?} must be accepted: {err}");
        }
    }

    fn refused(pattern: &str) -> Reason {
        match verdict(pattern) {
            Ok(_) => panic!("{pattern:?} must be refused"),
            Err(err) => {
                assert_eq!(err.location, "#", "{err}");
                err.reason
            }
        }
    }

    /// `{"properties": {"p0": {"pattern": …}, …}}` with `n` copies of `pattern`.
    fn patterns(n: usize, pattern: &str) -> Value {
        let properties: Map<String, Value> = (0..n)
            .map(|i| (format!("p{i}"), json!({ "pattern": pattern })))
            .collect();
        json!({ "properties": properties })
    }

    #[test]
    fn regexes_stay_in_the_shared_re2_subset() {
        // Go's parser refuses these as well.
        for pattern in [
            "(?=a)",
            "(a)\\1",
            "(?x)a b",
            "a{1001}",
            "\\Z",
            "a**",
            "a{2}{3}",
            "\\u0041",
            "(?P<a.b>x)",
            "(?P<é>x)",
            "[z-a]",
            "[a--]",
        ] {
            assert!(
                matches!(refused(pattern), Reason::InvalidRegex { .. }),
                "{pattern:?}"
            );
        }

        // Rust and Go read these differently, or only Go's Unicode tables can
        // size them: SDK-strict.
        for (pattern, rule) in [
            ("[a-z&&[^aeiou]]", Rule::ClassSetOperation),
            ("\\<", Rule::WordBoundaryExtension),
            ("\\p{L}", Rule::UnicodeClass),
        ] {
            assert_eq!(
                refused(pattern),
                Reason::SdkStrictRegex {
                    pattern: pattern.to_owned(),
                    rule
                }
            );
        }

        // `(?i)é` folds case outside ASCII; it is refused, as every inline flag
        // group is.
        assert!(matches!(refused("(?i)é"), Reason::InvalidRegex { .. }));

        // At most 1 024 bytes of source.
        assert_eq!(MAX_SCHEMA_REGEX_SOURCE_BYTES, 1_024);
        accepted(&"a".repeat(1_024));
        assert_eq!(
            refused(&"a".repeat(1_025)),
            Reason::RegexTooLong {
                bytes: 1_025,
                limit: 1_024
            }
        );

        // `\A` and `\z` mean the same in both.
        for pattern in [
            "^[A-Za-z0-9](?:[-A-Za-z0-9_.]{0,61}[A-Za-z0-9])?$",
            "\\S",
            "\\Aabc\\z",
            "\\A",
            "^\\d+(?:\\.\\d+)?$",
            "a*?b+?c??d{2}?",
            "\\bword\\B",
        ] {
            accepted(pattern);
        }
    }

    #[test]
    fn every_sdk_strict_rule_is_named_and_has_an_example() {
        let mut covered = HashSet::new();
        for (pattern, rule) in [
            ("\\pL", Rule::UnicodeClass),
            ("\\P{Han}", Rule::UnicodeClass),
            ("[\\p{Greek}a]", Rule::UnicodeClass),
            ("\\<", Rule::WordBoundaryExtension),
            ("\\>", Rule::WordBoundaryExtension),
            ("\\b{start}", Rule::WordBoundaryExtension),
            ("\\b{end-half}", Rule::WordBoundaryExtension),
            ("[a&&b]", Rule::ClassSetOperation),
            ("[a-z--b]", Rule::ClassSetOperation),
            ("[a~~b]", Rule::ClassSetOperation),
            ("[a[b]]", Rule::ClassReadDifferently),
            ("[--a]", Rule::ClassReadDifferently),
            ("a{ 2}", Rule::RepetitionReadAsLiteral),
            ("a{01}", Rule::RepetitionReadAsLiteral),
            ("a{2 ,3}", Rule::RepetitionReadAsLiteral),
        ] {
            assert_eq!(
                refused(pattern),
                Reason::SdkStrictRegex {
                    pattern: pattern.to_owned(),
                    rule
                },
                "{pattern:?}"
            );
            assert!(!rule.to_string().is_empty());
            covered.insert(rule);
        }
        for rule in Rule::ALL {
            assert!(covered.contains(rule), "{rule:?} has no example");
        }
    }

    #[test]
    fn inline_flag_groups_are_refused_wherever_their_text_appears() {
        // The check reads the source, so it also refuses the text of a flag
        // group where it is none: inside brackets, or after an escaped `(`.
        for pattern in [
            "(?i)a", "(?m)^a$", "(?s).", "(?U)a*", "(?-i)a", "(?i:a)", "(?im)a", "(?-s:.)",
            "[(?i)]", "\\(?id",
        ] {
            assert!(
                matches!(refused(pattern), Reason::InvalidRegex { ref why, .. } if why.contains("inline flag")),
                "{pattern:?}"
            );
        }
        for pattern in ["(?:a)", "(?P<n>a)", "(?<n>a)", "a(?:b|c)?"] {
            accepted(pattern);
        }
    }

    #[test]
    fn classes_that_read_the_same_in_both_are_accepted() {
        for pattern in [
            "[]a]",
            "[^]a]",
            "[a-]",
            "[-a]",
            "[\\]]",
            "[a-b-c]",
            "[[:alpha:][:digit:]_]",
            "[[:^space:]]",
            "[\\d\\s]",
            "[\\x00-\\x{10FFFF}]",
            "[.$^|(]",
            "[α-ω]",
        ] {
            accepted(pattern);
        }
    }

    #[test]
    fn some_go_syntax_is_refused_because_rust_cannot_parse_it() {
        // Go accepts each of these; regex-syntax does not parse them.
        for pattern in ["\\Q.\\E", "\\0", "x{,5}", "[\\d-z]", "(?)"] {
            assert!(
                matches!(refused(pattern), Reason::InvalidRegex { .. }),
                "{pattern:?}"
            );
        }
    }

    #[test]
    fn the_estimate_follows_gos_parse_tree() {
        for (pattern, size) in [
            ("", 3),
            ("a", 3),
            ("abc", 5),
            ("a*", 5),
            ("a+", 4),
            ("a?", 4),
            ("(a)", 5),
            ("^*", 5),
            ("\\Aabc\\z", 10),
            // One class of three runes.
            ("a|b|c", 3),
            // Go rewrites [Aa] as a case-folded literal; [Kk] stays two ranges,
            // since K also folds to the Kelvin sign.
            ("[Aa]", 3),
            ("[Kk]", 4),
            ("[Aa][Bb]", 4),
            ("(?:A|a)b", 6),
            // A one-rune class is a literal, and merges with its neighbours.
            ("[a]", 3),
            ("a[b]c", 5),
            // An empty group stays between two literals; a group's
            // concatenation is flattened, but its literals do not merge.
            ("a(?:)b", 8),
            ("a(?:bc*)d", 12),
            ("abc*d", 11),
            // Factoring can add to the size: a(?:|b), a(?:xx|yy).
            ("a|ab", 9),
            ("axx|ayy", 11),
            ("abc|abd", 7),
            ("a\\dx|a\\dy", 9),
            // a|b merge into [ab] before factoring sees them.
            ("ab|a|b", 7),
            // The top level is an alternation of one: [^\n] becomes `.`, and
            // a class of everything becomes any character.
            ("[^\\n]", 3),
            ("a[^\\n]", 7),
            ("[\\x00-\\x{10FFFF}]", 3),
            // Classes count their ranges.
            ("\\d", 3),
            ("\\D", 4),
            ("\\s", 5),
            ("\\w", 6),
            ("[[:alpha:]]", 4),
            ("[[:alpha:][:digit:]_]", 6),
            // Counted repetitions.
            ("x{0}", 3),
            ("x{2,5}", 10),
            ("x{2,}", 5),
            ("x{1,}", 5),
            ("x{0,}", 5),
            ("a{1000}", 1_002),
            ("^[a-z0-9]([-a-z0-9]*[a-z0-9])?$", 22),
            ("^[A-Za-z0-9](?:[-A-Za-z0-9_.]{0,61}[A-Za-z0-9])?$", 383),
        ] {
            assert_eq!(instructions(pattern), Ok(size), "{pattern:?}");
        }
    }

    #[test]
    fn nested_counted_repetitions_stay_within_1000_copies() {
        for pattern in ["((a{100}){100}){100}", "(a{4}){333}", "(a{1000}){2}"] {
            assert!(
                matches!(refused(pattern), Reason::InvalidRegex { .. }),
                "{pattern:?}"
            );
        }
        assert_eq!(instructions("(a{3}){333}"), Ok(1_667));
        // Go does not look inside what x{0} repeats.
        assert_eq!(instructions("((a{100}){0}){100}"), Ok(302));
    }

    #[test]
    fn regex_instruction_estimate_is_bounded_per_regex_and_per_schema() {
        assert_eq!(MAX_SCHEMA_REGEX_INSTRUCTIONS, 2_000);
        assert_eq!(MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL, 20_000);

        // a{n} is n, a concatenation adds its operand count, the program adds 2.
        assert_eq!(instructions("a{998}b{998}"), Ok(2_000));
        accepted("a{998}b{998}");
        assert_eq!(
            refused("a{998}b{999}"),
            Reason::RegexTooLarge {
                pattern: "a{998}b{999}".to_owned(),
                instructions: 2_001,
                limit: 2_000
            }
        );
        assert!(matches!(
            refused("a{1000}a{1000}"),
            Reason::RegexTooLarge {
                instructions: 2_004,
                ..
            }
        ));

        // Eleven patterns of 1 902 take the schema over 20 000; ten stay under.
        assert_eq!(instructions("a{950}b{948}"), Ok(1_902));
        WorkloadSchema::from_value(&patterns(10, "a{950}b{948}")).unwrap();
        let err = WorkloadSchema::from_value(&patterns(11, "a{950}b{948}")).unwrap_err();
        // Members are walked in name order, so p9 comes eleventh.
        assert_eq!(err.location, "#/properties/p9");
        assert_eq!(
            err.reason,
            Reason::SchemaRegexesTooLarge {
                instructions: 20_922,
                limit: 20_000
            }
        );

        // The same pattern in two places counts twice, and fits.
        WorkloadSchema::from_value(&json!({
            "pattern": "a{950}b{948}",
            "patternProperties": {"a{950}b{948}": true}
        }))
        .unwrap();
    }

    #[test]
    fn podiums_source_bound_covers_the_whole_schema() {
        assert_eq!(MAX_SCHEMA_REGEX_SOURCE_BYTES_TOTAL, 16_384);
        let letters = "x".repeat(1_024);
        WorkloadSchema::from_value(&patterns(16, &letters)).unwrap();
        let err = WorkloadSchema::from_value(&patterns(17, &letters)).unwrap_err();
        assert_eq!(
            err.reason,
            Reason::SchemaRegexesTooLong {
                bytes: 17 * 1_024,
                limit: 16_384
            }
        );
    }

    #[test]
    fn podium_counts_every_backslash_p_of_the_source() {
        assert_eq!(MAX_SCHEMA_REGEX_UNICODE_ESCAPES, 15);
        assert_eq!(MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL, 153);

        // `\\p` is a backslash and a `p`, which Podium still counts as a
        // Unicode class escape.
        let escapes = |n: usize| "\\\\p".repeat(n);
        accepted(&escapes(15));
        assert_eq!(
            refused(&escapes(16)),
            Reason::RegexTooManyUnicodeEscapes {
                pattern: escapes(16),
                count: 16,
                limit: 15
            }
        );

        // Podium charges 833 ranges for each, within 128 000 for a schema.
        WorkloadSchema::from_value(&patterns(5, &escapes(15))).unwrap();
        // Ten patterns of 15, then `q` with the rest: 153 fit, 154 do not.
        let schema = |last: usize| {
            let mut properties: Map<String, Value> = (0..10)
                .map(|i| (format!("p{i}"), json!({ "pattern": escapes(15) })))
                .collect();
            properties.insert("q".to_owned(), json!({ "pattern": escapes(last) }));
            json!({ "properties": properties })
        };
        WorkloadSchema::from_value(&schema(3)).unwrap();
        let err = WorkloadSchema::from_value(&schema(4)).unwrap_err();
        assert_eq!(err.location, "#/properties/q");
        assert_eq!(
            err.reason,
            Reason::SchemaRegexesTooManyUnicodeEscapes {
                count: 154,
                limit: 153
            }
        );
    }

    #[test]
    fn a_deeply_nested_pattern_within_1_kib_is_measured() {
        let pattern = format!("{}a{}", "(".repeat(511), ")".repeat(511));
        assert_eq!(pattern.len(), 1_023);
        // Each capture adds 2 to the literal's 1, and the program adds 2.
        assert_eq!(instructions(&pattern), Ok(1 + 511 * 2 + 2));
        accepted(&pattern);
    }
}
