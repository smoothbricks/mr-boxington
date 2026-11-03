//! Whether C or C++ text expands a macro whose value is the time it was
//! compiled: `__DATE__`, `__TIME__` or `__TIMESTAMP__`.
//!
//! An object built from such text is not a function of its inputs, so the
//! compilation is never cached. Naming a macro is not expanding it: a string
//! or character literal, a comment, or a longer identifier or number that holds
//! the name expands nothing. tinycc's token table
//! (`DEF(TOK___DATE__, "__DATE__")`), which every tinycc source includes, names
//! each one both ways, and a scan for the bytes alone left all of tinycc
//! uncached.
//!
//! Nearly no file names a macro at all, so the bytes are searched first
//! ([`Mentions`], one `memmem` per chunk while the file is hashed) and only a
//! file that holds them is lexed ([`Expansion`]).

/// Macros whose expansion is the time the compiler ran.
pub(super) const TIMESTAMP_MACROS: [&[u8]; 3] = [b"__DATE__", b"__TIME__", b"__TIMESTAMP__"];
const LONGEST_MACRO: usize = 13;
/// Identifiers that make a following `"` open a C++ raw string literal.
const RAW_STRING_PREFIXES: [&[u8]; 5] = [b"R", b"u8R", b"uR", b"UR", b"LR"];

/// Whether streamed bytes hold a macro's name anywhere.
#[derive(Default)]
pub(super) struct Mentions {
    window: Vec<u8>,
    found: bool,
}

impl Mentions {
    pub(super) fn feed(&mut self, chunk: &[u8]) {
        if self.found {
            return;
        }
        self.window.extend_from_slice(chunk);
        self.found = TIMESTAMP_MACROS
            .iter()
            .any(|name| memchr::memmem::find(&self.window, name).is_some());
        let keep = self.window.len().saturating_sub(LONGEST_MACRO - 1);
        self.window.drain(..keep);
    }

    pub(super) fn found(&self) -> bool {
        self.found
    }
}

/// Whether streamed text holds a macro's name as a preprocessing token of its
/// own, outside every literal and comment.
///
/// Line splices are removed first, as translation phase 2 does, so neither a
/// splice nor a backslash before a newline can make a literal look closed or
/// open. A literal the line does not close ends at the line, as compilers end
/// an unterminated one: an apostrophe in a skipped `#if 0` group then hides
/// nothing past its line. Raw string literals and trigraphs are not lexed:
/// either can make a quote mean something other than it looks, so a file that
/// holds one and names a macro anywhere counts as expanding it.
pub(super) struct Expansion {
    lexing: Lexing,
    token: Token,
    spelling: [u8; LONGEST_MACRO],
    spelled: usize,
    splice: Splice,
    question_marks: usize,
    expands: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lexing {
    Code,
    /// A `/` that may open a comment.
    Slash,
    LineComment,
    BlockComment {
        star: bool,
    },
    /// Inside a string (`"`) or character (`'`) literal.
    Literal {
        quote: u8,
        escaped: bool,
    },
    /// A `'` after a number: a digit separator when an identifier character
    /// follows (`1'000`), the start of a character literal otherwise.
    NumberQuote,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Token {
    None,
    Identifier,
    /// A preprocessing number (`0x1f`, `1.5e3`, `0__DATE__`): never a macro.
    Number,
}

/// Progress through a possible line splice: a backslash, horizontal
/// whitespace that compilers accept before the newline, then the newline.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Splice {
    None,
    Backslash { spaces: usize },
    BackslashCr,
}

impl Default for Expansion {
    fn default() -> Self {
        Self {
            lexing: Lexing::Code,
            token: Token::None,
            spelling: [0; LONGEST_MACRO],
            spelled: 0,
            splice: Splice::None,
            question_marks: 0,
            expands: false,
        }
    }
}

impl Expansion {
    pub(super) fn feed(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if self.expands {
                return;
            }
            self.trigraph(byte);
            self.unsplice(byte);
        }
    }

    pub(super) fn finish(mut self) -> bool {
        if let Splice::Backslash { spaces } = self.splice {
            self.lex(b'\\');
            (0..spaces).for_each(|_| self.lex(b' '));
        }
        self.lex(b'\n');
        self.expands
    }

    fn trigraph(&mut self, byte: u8) {
        if byte == b'?' {
            self.question_marks += 1;
            return;
        }
        if self.question_marks >= 2 && b"=/'()!<>-".contains(&byte) {
            self.expands = true;
        }
        self.question_marks = 0;
    }

    fn unsplice(&mut self, byte: u8) {
        match self.splice {
            Splice::None if byte == b'\\' => self.splice = Splice::Backslash { spaces: 0 },
            Splice::None => self.lex(byte),
            Splice::Backslash { spaces } => match byte {
                b'\n' => self.splice = Splice::None,
                b'\r' => self.splice = Splice::BackslashCr,
                b' ' | b'\t' | 0x0b | 0x0c => {
                    self.splice = Splice::Backslash { spaces: spaces + 1 };
                }
                _ => {
                    self.splice = Splice::None;
                    self.lex(b'\\');
                    (0..spaces).for_each(|_| self.lex(b' '));
                    self.unsplice(byte);
                }
            },
            // A backslash before a lone carriage return splices too: the
            // carriage return is a line end of its own.
            Splice::BackslashCr => {
                self.splice = Splice::None;
                if byte != b'\n' {
                    self.unsplice(byte);
                }
            }
        }
    }

    fn lex(&mut self, byte: u8) {
        match self.lexing {
            Lexing::Code => self.code(byte),
            Lexing::Slash => match byte {
                b'/' => self.lexing = Lexing::LineComment,
                b'*' => self.lexing = Lexing::BlockComment { star: false },
                _ => {
                    self.lexing = Lexing::Code;
                    self.code(byte);
                }
            },
            Lexing::LineComment => {
                if is_line_end(byte) {
                    self.lexing = Lexing::Code;
                }
            }
            Lexing::BlockComment { star } => {
                self.lexing = if star && byte == b'/' {
                    Lexing::Code
                } else {
                    Lexing::BlockComment { star: byte == b'*' }
                };
            }
            Lexing::Literal { quote, escaped } => {
                self.lexing = if escaped {
                    Lexing::Literal {
                        quote,
                        escaped: false,
                    }
                } else if byte == b'\\' {
                    Lexing::Literal {
                        quote,
                        escaped: true,
                    }
                } else if byte == quote || is_line_end(byte) {
                    Lexing::Code
                } else {
                    Lexing::Literal {
                        quote,
                        escaped: false,
                    }
                };
            }
            Lexing::NumberQuote => {
                if is_identifier(byte) {
                    self.lexing = Lexing::Code;
                    self.code(byte);
                } else {
                    self.token = Token::None;
                    self.lexing = Lexing::Literal {
                        quote: b'\'',
                        escaped: false,
                    };
                    self.lex(byte);
                }
            }
        }
    }

    fn code(&mut self, byte: u8) {
        if is_identifier(byte) {
            if self.token == Token::None {
                self.token = if byte.is_ascii_digit() {
                    Token::Number
                } else {
                    Token::Identifier
                };
                self.spelled = 0;
            }
            if let Some(slot) = self.spelling.get_mut(self.spelled) {
                *slot = byte;
            }
            self.spelled = self.spelled.saturating_add(1);
            return;
        }
        if self.token == Token::Number && byte == b'.' {
            return;
        }
        if self.token == Token::Number && byte == b'\'' {
            self.lexing = Lexing::NumberQuote;
            return;
        }
        let raw_string = byte == b'"'
            && self.token == Token::Identifier
            && RAW_STRING_PREFIXES.contains(&self.identifier());
        self.end_token();
        if raw_string {
            self.expands = true;
            return;
        }
        match byte {
            b'"' | b'\'' => {
                self.lexing = Lexing::Literal {
                    quote: byte,
                    escaped: false,
                };
            }
            b'/' => self.lexing = Lexing::Slash,
            _ => {}
        }
    }

    /// The identifier being lexed, or nothing when it is longer than any name
    /// compared against it.
    fn identifier(&self) -> &[u8] {
        self.spelling.get(..self.spelled).unwrap_or_default()
    }

    fn end_token(&mut self) {
        if self.token == Token::Identifier && TIMESTAMP_MACROS.contains(&self.identifier()) {
            self.expands = true;
        }
        self.token = Token::None;
    }
}

/// Identifier and preprocessing-number characters. Anything else, a `$` or a
/// non-ASCII byte included, ends a token: one compiler's longer identifier is
/// then a name of its own here, which can only count an expansion that is not
/// one.
fn is_identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_line_end(byte: u8) -> bool {
    byte == b'\n' || byte == b'\r'
}
