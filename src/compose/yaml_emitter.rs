//! A hand port of PyYAML's `safe_dump(sort_keys=True, default_flow_style=False)`
//! emitter, reproducing its output byte-for-byte.
//!
//! `compose.lock.yml` is embedded in both the initramfs and the `.tdvmm`, and its
//! bytes are a byte-identity acceptance gate against the retired Python producer,
//! which emitted the lock with `yaml.safe_dump`. libyaml-based crates do not
//! reproduce PyYAML's exact wrapping and quoting, so this module ports the pieces
//! that decide it — `analyze_scalar` / `choose_scalar_style` / `write_plain` /
//! `write_single_quoted` / `write_double_quoted` and the block layout — with PyYAML's
//! `best_indent=2`, `best_width=80`, `allow_unicode=False` defaults. Parsing stays on
//! `serde_yaml`; only emission is ours.
//!
//! [`emit_yaml`] is the entry point. Scalar rendering ([`scalar_repr`] /
//! [`format_float`]) mirrors PyYAML's `SafeRepresenter`; [`float_is_representable`]
//! marks the one range where `repr(float)` and Rust's `Display` diverge, which
//! `validate` rejects rather than emit a lock that silently disagrees.

use serde_yaml::Value;

const BEST_INDENT: i64 = 2;
const BEST_WIDTH: i64 = 80;

/// The scalar's YAML 1.1 core tag as PyYAML's default resolver would detect it
/// from the *plain* rendering — this drives whether a value can stay unquoted.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Tag {
    Str,
    Int,
    Float,
    Bool,
    Null,
    Other, // merge/value/yaml — irrelevant here, never our target tag
}

/// PyYAML's default implicit resolver, restricted to what a plain scalar can
/// resolve to. Mirrors `Resolver.yaml_implicit_resolvers` (YAML 1.1 schema, still
/// shipped by PyYAML 6): bool includes yes/no/on/off and case variants.
fn resolve_plain(value: &str) -> Tag {
    if value.is_empty() {
        return Tag::Null; // the empty branch of the null pattern
    }
    let b0 = value.as_bytes()[0];
    // The resolver only consults patterns whose first-char index contains b0;
    // checking all is equivalent since the regexes are anchored.
    // null: ~ | null | Null | NULL | (empty handled above)
    match value {
        "~" | "null" | "Null" | "NULL" => return Tag::Null,
        _ => {}
    }
    match value {
        "yes" | "Yes" | "YES" | "no" | "No" | "NO" | "true" | "True" | "TRUE"
        | "false" | "False" | "FALSE" | "on" | "On" | "ON" | "off" | "Off" | "OFF" => {
            return Tag::Bool
        }
        _ => {}
    }
    // Fast reject: int/float/timestamp all start with a digit, sign, or dot.
    let numeric_lead = b0.is_ascii_digit() || b0 == b'-' || b0 == b'+' || b0 == b'.';
    if numeric_lead {
        if int_re(value) {
            return Tag::Int;
        }
        if float_re(value) {
            return Tag::Float;
        }
        if timestamp_re(value) {
            return Tag::Other; // timestamp tag; != str, forces quoting
        }
    }
    Tag::Str
}

/// `^(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+
///    |[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)$`
fn int_re(s: &str) -> bool {
    let t = s.strip_prefix(['-', '+']).unwrap_or(s);
    if t.is_empty() {
        return false;
    }
    // 0b binary
    if let Some(r) = t.strip_prefix("0b") {
        return !r.is_empty() && r.bytes().all(|c| c == b'0' || c == b'1' || c == b'_');
    }
    // 0x hex
    if let Some(r) = t.strip_prefix("0x") {
        return !r.is_empty() && r.bytes().all(|c| c.is_ascii_hexdigit() || c == b'_');
    }
    // sexagesimal int: [1-9][0-9_]*(:[0-5]?[0-9])+
    if t.contains(':') {
        let mut parts = t.split(':');
        let first = parts.next().unwrap();
        if !(first.as_bytes()[0].is_ascii_digit()
            && first.as_bytes()[0] != b'0'
            && first.bytes().all(|c| c.is_ascii_digit() || c == b'_'))
        {
            return false;
        }
        let mut any = false;
        for p in parts {
            any = true;
            if p.is_empty() || p.len() > 2 || !p.bytes().all(|c| c.is_ascii_digit()) {
                return false;
            }
            if p.len() == 2 && !(b'0'..=b'5').contains(&p.as_bytes()[0]) {
                return false;
            }
        }
        return any;
    }
    // octal: 0[0-7_]+
    if t.starts_with('0') && t.len() > 1 {
        return t[1..].bytes().all(|c| (b'0'..=b'7').contains(&c) || c == b'_');
    }
    // plain: 0 | [1-9][0-9_]*
    if t == "0" {
        return true;
    }
    let tb = t.as_bytes();
    (b'1'..=b'9').contains(&tb[0]) && tb.iter().all(|&c| c.is_ascii_digit() || c == b'_')
}

/// A pragmatic port of PyYAML's float implicit pattern (enough for the subset —
/// any value that looks floaty must be quoted if it is meant as a string).
fn float_re(s: &str) -> bool {
    let t = s.strip_prefix(['-', '+']).unwrap_or(s);
    match t {
        ".inf" | ".Inf" | ".INF" => return true,
        _ => {}
    }
    match s {
        ".nan" | ".NaN" | ".NAN" => return true,
        _ => {}
    }
    // [0-9][0-9_]*\.[0-9_]*(e...)? | \.[0-9][0-9_]*(e...)?  (+ sexagesimal float)
    if !t.contains('.') {
        return false;
    }
    // Split off an exponent if present.
    let (mantissa, exp_ok) = match t.split_once(['e', 'E']) {
        Some((m, e)) => {
            let e = e.strip_prefix(['-', '+']).unwrap_or("");
            (m, !e.is_empty() && e.bytes().all(|c| c.is_ascii_digit()))
        }
        None => (t, true),
    };
    if !exp_ok {
        return false;
    }
    let Some((intp, frac)) = mantissa.split_once('.') else {
        return false;
    };
    let int_ok = !intp.is_empty()
        && intp.as_bytes()[0].is_ascii_digit()
        && intp.bytes().all(|c| c.is_ascii_digit() || c == b'_');
    let lead_dot_ok = intp.is_empty() // ".5"
        && !frac.is_empty()
        && frac.as_bytes()[0].is_ascii_digit();
    let frac_ok = frac.bytes().all(|c| c.is_ascii_digit() || c == b'_');
    (int_ok && frac_ok) || lead_dot_ok
}

/// YYYY-MM-DD date / full timestamp lead (only needs to be conservative).
fn timestamp_re(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 8
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4] == b'-'
}

/// Result of `analyze_scalar` — which styles are legal for a given string. Some
/// fields are unused by our block-only, safe_dump emitter but kept for a faithful
/// 1:1 port of PyYAML's `analyze_scalar`.
#[allow(dead_code)]
struct Analysis {
    empty: bool,
    multiline: bool,
    allow_flow_plain: bool,
    allow_block_plain: bool,
    allow_single_quoted: bool,
    allow_double_quoted: bool,
    allow_block: bool,
}

/// A direct port of `Emitter.analyze_scalar` (allow_unicode=False).
fn analyze_scalar(scalar: &str) -> Analysis {
    if scalar.is_empty() {
        return Analysis {
            empty: true,
            multiline: false,
            allow_flow_plain: false,
            allow_block_plain: true,
            allow_single_quoted: true,
            allow_double_quoted: true,
            allow_block: false,
        };
    }
    let chars: Vec<char> = scalar.chars().collect();
    let n = chars.len();

    let mut block_indicators = false;
    let mut flow_indicators = false;
    let mut line_breaks = false;
    let mut special_characters = false;

    let mut leading_space = false;
    let mut leading_break = false;
    let mut trailing_space = false;
    let mut trailing_break = false;
    let mut break_space = false;
    let mut space_break = false;

    if scalar.starts_with("---") || scalar.starts_with("...") {
        block_indicators = true;
        flow_indicators = true;
    }

    let is_ws = |c: char| matches!(c, '\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');
    let is_break = |c: char| matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');

    let mut preceded_by_whitespace = true;
    let mut followed_by_whitespace = n == 1 || is_ws(chars[1]);
    let mut previous_space = false;
    let mut previous_break = false;

    let mut index = 0usize;
    while index < n {
        let ch = chars[index];
        if index == 0 {
            if matches!(ch, '#' | ',' | '[' | ']' | '{' | '}' | '&' | '*' | '!' | '|' | '>' | '\'' | '"' | '%' | '@' | '`') {
                flow_indicators = true;
                block_indicators = true;
            }
            if matches!(ch, '?' | ':') {
                flow_indicators = true;
                if followed_by_whitespace {
                    block_indicators = true;
                }
            }
            if ch == '-' && followed_by_whitespace {
                flow_indicators = true;
                block_indicators = true;
            }
        } else {
            if matches!(ch, ',' | '?' | '[' | ']' | '{' | '}') {
                flow_indicators = true;
            }
            if ch == ':' {
                flow_indicators = true;
                if followed_by_whitespace {
                    block_indicators = true;
                }
            }
            if ch == '#' && preceded_by_whitespace {
                flow_indicators = true;
                block_indicators = true;
            }
        }

        if is_break(ch) {
            line_breaks = true;
        }
        if !(ch == '\n' || ('\x20'..='\x7e').contains(&ch)) {
            // allow_unicode = False: any non-printable-ASCII (besides '\n') is special.
            special_characters = true;
        }

        if ch == ' ' {
            if index == 0 {
                leading_space = true;
            }
            if index == n - 1 {
                trailing_space = true;
            }
            if previous_break {
                break_space = true;
            }
            previous_space = true;
            previous_break = false;
        } else if is_break(ch) {
            if index == 0 {
                leading_break = true;
            }
            if index == n - 1 {
                trailing_break = true;
            }
            if previous_space {
                space_break = true;
            }
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }

        index += 1;
        preceded_by_whitespace = is_ws(ch);
        followed_by_whitespace = index + 1 >= n || is_ws(chars[index + 1]);
    }

    let mut allow_flow_plain = true;
    let mut allow_block_plain = true;
    let mut allow_single_quoted = true;
    let allow_double_quoted = true;
    let mut allow_block = true;

    if leading_space || leading_break || trailing_space || trailing_break {
        allow_flow_plain = false;
        allow_block_plain = false;
    }
    if trailing_space {
        allow_block = false;
    }
    if break_space {
        allow_flow_plain = false;
        allow_block_plain = false;
        allow_single_quoted = false;
    }
    if space_break || special_characters {
        allow_flow_plain = false;
        allow_block_plain = false;
        allow_single_quoted = false;
        allow_block = false;
    }
    if line_breaks {
        allow_flow_plain = false;
        allow_block_plain = false;
    }
    if flow_indicators {
        allow_flow_plain = false;
    }
    if block_indicators {
        allow_block_plain = false;
    }

    Analysis {
        empty: false,
        multiline: line_breaks,
        allow_flow_plain,
        allow_block_plain,
        allow_single_quoted,
        allow_double_quoted,
        allow_block,
    }
}

const ESCAPE_REPLACEMENTS: &[(char, char)] = &[
    ('\0', '0'),
    ('\x07', 'a'),
    ('\x08', 'b'),
    ('\t', 't'),
    ('\n', 'n'),
    ('\x0b', 'v'),
    ('\x0c', 'f'),
    ('\r', 'r'),
    ('\x1b', 'e'),
    ('"', '"'),
    ('\\', '\\'),
    ('\u{85}', 'N'),
    ('\u{a0}', '_'),
    ('\u{2028}', 'L'),
    ('\u{2029}', 'P'),
];

/// The emitter state machine (the subset of `yaml.emitter.Emitter` we need).
struct Emitter {
    out: Vec<u8>,
    column: i64,
    indent: Option<i64>,
    indents: Vec<Option<i64>>,
    whitespace: bool,
    indention: bool,
}

impl Emitter {
    fn new() -> Emitter {
        Emitter {
            out: Vec::new(),
            column: 0,
            indent: None,
            indents: Vec::new(),
            whitespace: true,
            indention: true,
        }
    }

    fn write(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }

    fn write_line_break(&mut self) {
        self.whitespace = true;
        self.indention = true;
        self.column = 0;
        self.out.push(b'\n');
    }

    fn write_indent(&mut self) {
        let indent = self.indent.unwrap_or(0);
        if !self.indention
            || self.column > indent
            || (self.column == indent && !self.whitespace)
        {
            self.write_line_break();
        }
        if self.column < indent {
            self.whitespace = true;
            let pad = (indent - self.column) as usize;
            self.out.extend(std::iter::repeat_n(b' ', pad));
            self.column = indent;
        }
    }

    fn write_indicator(&mut self, indicator: &str, need_whitespace: bool, whitespace: bool, indention: bool) {
        let data = if self.whitespace || !need_whitespace {
            indicator.to_string()
        } else {
            format!(" {indicator}")
        };
        self.whitespace = whitespace;
        self.indention = self.indention && indention;
        self.column += data.chars().count() as i64;
        self.write(&data);
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        match self.indent {
            None => self.indent = Some(if flow { BEST_INDENT } else { 0 }),
            Some(i) if !indentless => self.indent = Some(i + BEST_INDENT),
            _ => {}
        }
    }

    fn write_plain(&mut self, text: &str, split: bool) {
        if text.is_empty() {
            return;
        }
        if !self.whitespace {
            self.column += 1;
            self.out.push(b' ');
        }
        self.whitespace = false;
        self.indention = false;
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let is_break = |c: char| matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');
        let mut spaces = false;
        let mut breaks = false;
        let mut start = 0usize;
        let mut end = 0usize;
        while end <= n {
            let ch = if end < n { Some(chars[end]) } else { None };
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column > BEST_WIDTH && split {
                        self.write_indent();
                        self.whitespace = false;
                        self.indention = false;
                    } else {
                        let data: String = chars[start..end].iter().collect();
                        self.column += (end - start) as i64;
                        self.write(&data);
                    }
                    start = end;
                }
            } else if breaks {
                if !matches!(ch, Some(c) if is_break(c)) {
                    // (no fully-plain multiline in our data; keep faithful anyway)
                    if chars[start] == '\n' {
                        self.write_line_break();
                    }
                    for _ in start..end {
                        self.write_line_break();
                    }
                    self.write_indent();
                    self.whitespace = false;
                    self.indention = false;
                    start = end;
                }
            } else if ch.is_none() || matches!(ch, Some(c) if c == ' ' || is_break(c)) {
                let data: String = chars[start..end].iter().collect();
                self.column += (end - start) as i64;
                self.write(&data);
                start = end;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
            end += 1;
        }
    }

    fn write_single_quoted(&mut self, text: &str, split: bool) {
        self.write_indicator("'", true, false, false);
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let is_break = |c: char| matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');
        let mut spaces = false;
        let mut breaks = false;
        let mut start = 0usize;
        let mut end = 0usize;
        while end <= n {
            let ch = if end < n { Some(chars[end]) } else { None };
            if spaces {
                if ch.is_none() || ch != Some(' ') {
                    if start + 1 == end && self.column > BEST_WIDTH && split && start != 0 && end != n {
                        self.write_indent();
                    } else {
                        let data: String = chars[start..end].iter().collect();
                        self.column += (end - start) as i64;
                        self.write(&data);
                    }
                    start = end;
                }
            } else if breaks {
                if ch.is_none() || !matches!(ch, Some(c) if is_break(c)) {
                    if chars[start] == '\n' {
                        self.write_line_break();
                    }
                    for &br in &chars[start..end] {
                        let _ = br;
                        self.write_line_break();
                    }
                    self.write_indent();
                    start = end;
                }
            } else if (ch.is_none() || matches!(ch, Some(c) if c == ' ' || is_break(c) || c == '\''))
                && start < end
            {
                let data: String = chars[start..end].iter().collect();
                self.column += (end - start) as i64;
                self.write(&data);
                start = end;
            }
            if ch == Some('\'') {
                self.column += 2;
                self.write("''");
                start = end + 1;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
            end += 1;
        }
        self.write_indicator("'", false, false, false);
    }

    fn write_double_quoted(&mut self, text: &str, split: bool) {
        self.write_indicator("\"", true, false, false);
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let mut start = 0usize;
        let mut end = 0usize;
        while end <= n {
            let ch = if end < n { Some(chars[end]) } else { None };
            let needs_escape = |c: char| -> bool {
                matches!(c, '"' | '\\' | '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{feff}')
                    || !('\x20'..='\x7e').contains(&c) // allow_unicode = False
            };
            if ch.is_none() || matches!(ch, Some(c) if needs_escape(c)) {
                if start < end {
                    let data: String = chars[start..end].iter().collect();
                    self.column += (end - start) as i64;
                    self.write(&data);
                    start = end;
                }
                if let Some(c) = ch {
                    let data = if let Some(&(_, r)) = ESCAPE_REPLACEMENTS.iter().find(|&&(k, _)| k == c) {
                        format!("\\{r}")
                    } else if (c as u32) <= 0xff {
                        format!("\\x{:02X}", c as u32)
                    } else if (c as u32) <= 0xffff {
                        format!("\\u{:04X}", c as u32)
                    } else {
                        format!("\\U{:08X}", c as u32)
                    };
                    self.column += data.chars().count() as i64;
                    self.write(&data);
                    start = end + 1;
                }
            }
            if 0 < end
                && end < n.saturating_sub(1)
                && (ch == Some(' ') || start >= end)
                && self.column + (end as i64 - start as i64) > BEST_WIDTH
                && split
            {
                // Python slicing is lenient: text[start:end] is "" when start>=end.
                let mut data: String = if start < end {
                    chars[start..end].iter().collect()
                } else {
                    String::new()
                };
                data.push('\\');
                if start < end {
                    start = end;
                }
                self.column += data.chars().count() as i64;
                self.write(&data);
                self.write_indent();
                self.whitespace = false;
                self.indention = false;
                if chars.get(start) == Some(&' ') {
                    self.column += 1;
                    self.write("\\");
                }
            }
            end += 1;
        }
        self.write_indicator("\"", false, false, false);
    }

    /// Emit one scalar node (value already rendered to its plain text + target tag).
    fn emit_scalar(&mut self, text: &str, tag: Tag, simple_key: bool) {
        let analysis = analyze_scalar(text);
        // implicit[0]: would the plain rendering resolve back to this exact tag?
        let implicit0 = resolve_plain(text) == tag;
        // choose_scalar_style (style is always None for safe_dump).
        let style = self.choose_style(&analysis, implicit0, simple_key);
        // expect_scalar: increase_indent(flow=True) around the write.
        self.increase_indent(true, false);
        let split = !simple_key;
        match style {
            ScalarStyle::Plain => self.write_plain(text, split),
            ScalarStyle::Single => self.write_single_quoted(text, split),
            ScalarStyle::Double => self.write_double_quoted(text, split),
        }
        self.indent = self.indents.pop().unwrap();
    }

    fn choose_style(&self, a: &Analysis, implicit0: bool, simple_key: bool) -> ScalarStyle {
        // flow_level is always 0 (block); canonical=False; event.style=None.
        if implicit0
            && !(simple_key && (a.empty || a.multiline))
            && a.allow_block_plain
        {
            return ScalarStyle::Plain;
        }
        if a.allow_single_quoted && !(simple_key && a.multiline) {
            return ScalarStyle::Single;
        }
        ScalarStyle::Double
    }
}

#[derive(Clone, Copy)]
enum ScalarStyle {
    Plain,
    Single,
    Double,
}

/// Render a `serde_yaml::Value` scalar to (plain-text, target-tag) exactly as
/// PyYAML's SafeRepresenter would. `None` for a composite (mapping/sequence) or a
/// custom-tagged node, which have no plain scalar rendering.
fn scalar_repr(v: &Value) -> Option<(String, Tag)> {
    match v {
        Value::Null => Some(("null".to_string(), Tag::Null)),
        Value::Bool(b) => Some(((if *b { "true" } else { "false" }).to_string(), Tag::Bool)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some((i.to_string(), Tag::Int))
            } else if let Some(u) = n.as_u64() {
                Some((u.to_string(), Tag::Int))
            } else if let Some(f) = n.as_f64() {
                Some((format_float(f), Tag::Float))
            } else {
                Some((n.to_string(), Tag::Float))
            }
        }
        Value::String(s) => Some((s.clone(), Tag::Str)),
        _ => None,
    }
}

/// Whether [`format_float`] reproduces PyYAML's `repr(float)` for `f`.
///
/// PyYAML (CPython `repr`) switches to scientific notation once a finite value needs
/// more than 16 integer digits (`|f| >= 1e16`) or has four or more leading zeros
/// after the point (`0 < |f| < 1e-4`); Rust's `Display` never does, so a lock emitted
/// in those ranges would silently disagree with the Python producer. Inside the
/// fixed-notation range both use the same shortest round-tripping digits, so
/// [`format_float`] matches exactly. `validate` rejects any float outside this range
/// rather than emit a divergent lock; `.inf`/`.nan` render identically and are safe.
pub(super) fn float_is_representable(f: f64) -> bool {
    if !f.is_finite() {
        return true;
    }
    let a = f.abs();
    a == 0.0 || (1e-4..1e16).contains(&a)
}

/// Render a float as PyYAML `represent_float` (CPython `repr`) would. Exact for the
/// range [`float_is_representable`] admits — the only range `validate` lets reach the
/// emitter — appending `.0` to an integral value as `repr` does.
fn format_float(f: f64) -> String {
    if f.is_nan() {
        return ".nan".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { ".inf".to_string() } else { "-.inf".to_string() };
    }
    let mut s = format!("{f}");
    if !s.contains('.') && !s.contains('e') && !s.contains('E') {
        s.push_str(".0");
    }
    s
}

/// Emit `value` as PyYAML `safe_dump(value, sort_keys=True,
/// default_flow_style=False)` would — byte-for-byte.
pub(super) fn emit_yaml(value: &Value) -> Vec<u8> {
    let mut e = Emitter::new();
    emit_node(&mut e, value, false);
    // expect_document_end -> write_indent() -> trailing newline.
    e.write_indent();
    e.out
}

/// expect_node for block context (root/mapping-value/sequence-item). `mapping` marks a
/// mapping-value position, which controls indentless block sequences.
fn emit_node(e: &mut Emitter, value: &Value, mapping: bool) {
    match value {
        Value::Mapping(m) => {
            if m.is_empty() {
                // check_empty_mapping -> flow "{}"
                e.write_indicator("{", true, true, false);
                e.write_indicator("}", false, false, false);
            } else {
                emit_block_mapping(e, m);
            }
        }
        Value::Sequence(s) => {
            if s.is_empty() {
                e.write_indicator("[", true, true, false);
                e.write_indicator("]", false, false, false);
            } else {
                emit_block_sequence(e, s, mapping);
            }
        }
        // A custom-tagged node has no plain scalar rendering. `validate` rejects
        // custom tags before emit, so this never runs in the pipeline; emit the
        // underlying value so the emitter stays total (never panics) even when
        // called directly on tagged input.
        Value::Tagged(t) => emit_node(e, &t.value, mapping),
        scalar => {
            if let Some((text, tag)) = scalar_repr(scalar) {
                e.emit_scalar(&text, tag, false);
            }
        }
    }
}

fn sorted_keys(m: &serde_yaml::Mapping) -> Vec<(String, &Value)> {
    let mut items: Vec<(String, &Value)> = Vec::with_capacity(m.len());
    for (k, v) in m {
        // Keys in our compose docs are always strings.
        let ks = match k {
            Value::String(s) => s.clone(),
            other => scalar_repr(other).map(|(t, _)| t).unwrap_or_default(),
        };
        items.push((ks, v));
    }
    // sort_keys=True: Python sorts (key, value) tuples; keys are unique strings.
    items.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    items
}

fn emit_block_mapping(e: &mut Emitter, m: &serde_yaml::Mapping) {
    e.increase_indent(false, false);
    let items = sorted_keys(m);
    for (k, v) in items {
        e.write_indent();
        // check_simple_key: our keys are short, non-empty, single-line -> simple.
        e.emit_scalar(&k, Tag::Str, true);
        // expect_block_mapping_simple_value: ':' with need_whitespace=False.
        e.write_indicator(":", false, false, false);
        emit_node(e, v, true);
    }
    e.indent = e.indents.pop().unwrap();
}

fn emit_block_sequence(e: &mut Emitter, s: &[Value], mapping_context: bool) {
    let indentless = mapping_context && !e.indention;
    e.increase_indent(false, indentless);
    for item in s {
        e.write_indent();
        e.write_indicator("-", true, false, true);
        emit_node(e, item, false);
    }
    e.indent = e.indents.pop().unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every committed corpus `compose.lock.yml` IS PyYAML output. Strip the header
    /// comment lines, parse with serde_yaml, re-emit with our port, and require
    /// byte-identical output. This pins emitter fidelity against the real corpus.
    #[test]
    fn round_trips_committed_locks_byte_for_byte() {
        let stacks = ["insert-trim", "demo", "pgcluster", "tigerbeetle"];
        for stack in stacks {
            let path = format!("testdata/stacks/{stack}/compose.lock.yml");
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {path}: {e}"));
            // Strip leading '#' comment lines (the generated header).
            let mut body = String::new();
            let mut in_header = true;
            for line in raw.lines() {
                if in_header && line.starts_with('#') {
                    continue;
                }
                in_header = false;
                body.push_str(line);
                body.push('\n');
            }
            let value: Value = serde_yaml::from_str(&body)
                .unwrap_or_else(|e| panic!("parse {path}: {e}"));
            let emitted = emit_yaml(&value);
            let emitted_s = String::from_utf8_lossy(&emitted);
            assert_eq!(
                emitted_s, body,
                "emitter mismatch for {stack}\n--- expected ---\n{body}\n--- got ---\n{emitted_s}"
            );
        }
    }

    /// The emitter is total: a custom-tagged node (which `validate` rejects upstream)
    /// emits its underlying value instead of hitting the old `expect` panic.
    #[test]
    fn emit_yaml_is_total_on_a_tagged_node() {
        let v: Value = serde_yaml::from_str("a: !secret hunter2\n").unwrap();
        let out = String::from_utf8(emit_yaml(&v)).unwrap();
        assert_eq!(out, "a: hunter2\n");
    }

    /// `format_float` reproduces CPython `repr(float)` across the fixed-notation range
    /// `validate` admits — integral floats gain `.0`, and no value switches to
    /// scientific notation.
    #[test]
    fn format_float_matches_python_repr_in_range() {
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.5, "1.5"),
            (-2.5, "-2.5"),
            (100.0, "100.0"),
            (3.14, "3.14"),
            (0.1, "0.1"),
            (0.0001, "0.0001"),
            (1234.5, "1234.5"),
            (f64::INFINITY, ".inf"),
            (f64::NEG_INFINITY, "-.inf"),
            (f64::NAN, ".nan"),
        ];
        for (f, want) in cases {
            assert_eq!(format_float(f), want, "format_float({f})");
        }
    }

    /// The representability boundary matches PyYAML's scientific-notation switch:
    /// `|f| >= 1e16` or `0 < |f| < 1e-4` diverge (rejected); everything else, plus
    /// zero and the non-finite values, reproduces.
    #[test]
    fn float_representability_boundary() {
        // Diverges — PyYAML would emit scientific notation.
        for f in [1e16, 2.5e16, 1e20, 1e-5, 1e-10, -1e16, -1e-5] {
            assert!(!float_is_representable(f), "{f} should be rejected");
        }
        // Reproduces — fixed notation with identical digits.
        for f in [0.0, -0.0, 1e-4, 9.99e15, 1.5, 100.0, -3.25, f64::INFINITY, f64::NAN] {
            assert!(float_is_representable(f), "{f} should be representable");
        }
    }
}
