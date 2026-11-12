//! Repeated parameter groups (`[params.<group>] repeat = N`, P2 of the 1.3.1 plan).
//!
//! ```toml
//! [params.band]
//! repeat    = 6              # how many copies (1 to 1024)
//! key       = "band{n}_"     # `prefix` means the same; {n} and {n:02} (zero padded)
//! name      = "Band {n} "    # put before each field's name
//! start     = 1              # the first {n} (default 1)
//! id_base   = 7              # IDs: id_base + (n - start) * id_stride + offset
//! id_stride = 17
//!
//! [params.band.fields]
//! freq = { type = "float", range = [20, 20000], scale = "log", offset = 0 }
//! gain = { type = "float", range = [-18, 18], unit = "dB", smooth = "20ms", offset = 1 }
//! ```
//!
//! The group expands to `repeat * fields` ordinary parameters, copy by copy, each copy in
//! the order the fields are written: `band1_freq`, `band1_gain`, `band2_freq`, ... Without
//! `id_base` the IDs are the hashes of the keys, as for any parameter. `id_base` may also
//! be an expression of `n` (`"2000 + (n - 1) * 64"`); `offset` is then added to it.

use super::*;

const MAX_REPEAT: i64 = 1024;

pub(crate) fn parse_group(name: &str, t: &Table, groups: &mut Vec<Group>, params: &mut Vec<Param>) -> Result<(), Error> {
    let at = format!("params.{name}");
    if !is_ident(name) {
        return Err(error!("bad-param-key", "{$key}: parameter keys must be snake_case identifiers (lowercase letters, digits and _)", key = name));
    }
    reject_unknown(t, &format!("{at}."), &["repeat", "key", "prefix", "name", "start", "id_base", "id_stride", "fields"])?;
    let repeat = t
        .get("repeat")
        .and_then(Value::as_integer)
        .filter(|n| (1..=MAX_REPEAT).contains(n))
        .ok_or_else(|| error!("bad-group-repeat", "{$at} must be an integer from 1 to {$max}", at = format!("{at}.repeat"), max = MAX_REPEAT))? as usize;
    let start = match t.get("start") {
        None => 1,
        Some(v) => v.as_integer().filter(|n| (0..=1_000_000).contains(n)).ok_or_else(|| {
            error!("bad-group-start", "{$at} must be a whole number from 0 to 1000000", at = format!("{at}.start"))
        })?,
    };
    let key_template = match (t.get("key"), t.get("prefix")) {
        (Some(_), Some(_)) => return Err(error!("group-key-twice", "{$at}: write `key` or `prefix`, not both (they mean the same)", at = at)),
        (Some(v), None) | (None, Some(v)) => v.as_str().ok_or_else(|| not_string(&format!("{at}.key")))?.to_string(),
        (None, None) => format!("{name}_{{n}}_"),
    };
    let name_template = match t.get("name") {
        None => None,
        Some(v) => Some(v.as_str().ok_or_else(|| not_string(&format!("{at}.name")))?.to_string()),
    };
    let fields = t
        .get("fields")
        .and_then(Value::as_table)
        .filter(|f| !f.is_empty())
        .ok_or_else(|| error!("group-fields-required", "{$at} is required: a table with at least one field", at = format!("{at}.fields")))?;

    // IDs: none (hashed keys), an integer base with a stride, or an expression of n.
    enum Base {
        None,
        Number(i64, Option<i64>),
        Expr(String),
    }
    let id_stride = match t.get("id_stride") {
        None => None,
        Some(v) => Some(v.as_integer().filter(|n| *n >= 0).ok_or_else(|| {
            error!("bad-group-stride", "{$at} must be a whole number of 0 or more", at = format!("{at}.id_stride"))
        })?),
    };
    let base = match t.get("id_base") {
        None => {
            if id_stride.is_some() {
                return Err(error!("group-stride-needs-base", "{$at} needs `id_base`", at = format!("{at}.id_stride")));
            }
            Base::None
        }
        Some(Value::Integer(n)) => Base::Number(*n, id_stride),
        Some(Value::String(s)) => {
            if id_stride.is_some() {
                return Err(error!("group-stride-with-expression", "{$at}: `id_stride` goes with a number in `id_base`; an expression already has `n`", at = at));
            }
            // check the expression now, with the first n, so a typo is reported at the group
            eval(s, start).map_err(|why| error!("bad-group-expression", "{$at}: {$why}", at = format!("{at}.id_base"), why = why))?;
            Base::Expr(s.clone())
        }
        Some(_) => return Err(error!("bad-group-base", "{$at} must be a number or a string such as \"2000 + (n-1)*64\"", at = format!("{at}.id_base"))),
    };
    if let Base::Number(_, None) = base {
        if repeat > 1 {
            return Err(error!("group-needs-stride", "{$at} is needed when `id_base` is a number and `repeat` is more than 1", at = format!("{at}.id_stride")));
        }
    }

    let group_index = groups.len();
    let first = params.len();
    let mut field_keys = Vec::new();
    // Parse every field once with the first copy's key, so a mistake is reported once, not N times.
    for (instance, n) in (0..repeat).map(|i| (i, start + i as i64)) {
        for (f, (field, spec)) in fields.iter().enumerate() {
            let fat = format!("{at}.fields.{field}");
            let mut ft = spec.as_table().cloned().ok_or_else(|| not_table(&fat))?;
            if ft.contains_key("id") {
                return Err(error!("group-field-id", "{$at}: write `offset` (with `id_base` on the group) instead of `id`", at = format!("{fat}.id")));
            }
            let offset = match ft.remove("offset") {
                None => None,
                Some(v) => Some(v.as_integer().filter(|n| *n >= 0).ok_or_else(|| {
                    error!("bad-group-offset", "{$at} must be a whole number of 0 or more", at = format!("{fat}.offset"))
                })?),
            };
            if !is_ident(field) {
                return Err(error!("bad-param-key", "{$key}: parameter keys must be snake_case identifiers (lowercase letters, digits and _)", key = field));
            }
            let key = format!("{}{}", expand(&key_template, n, &format!("{at}.key"))?, field);
            if let Some(tpl) = &name_template {
                let own = match ft.get("name") {
                    Some(v) => v.as_str().ok_or_else(|| not_string(&format!("{fat}.name")))?.to_string(),
                    None => title_case(field),
                };
                ft.insert("name".into(), Value::String(format!("{}{}", expand(tpl, n, &format!("{at}.name"))?, own)));
            }
            let mut p = parse_param(&key, &Value::Table(ft))?;
            let id = match (&base, offset) {
                (Base::None, None) => None,
                (Base::None, Some(_)) => {
                    return Err(error!("group-offset-needs-base", "{$at}: `offset` needs `id_base` on the group", at = format!("{fat}.offset")));
                }
                (_, None) => return Err(error!("group-offset-required", "{$at} is required when the group has `id_base`", at = format!("{fat}.offset"))),
                (Base::Number(b, stride), Some(o)) => Some(b + instance as i64 * stride.unwrap_or(0) + o),
                (Base::Expr(e), Some(o)) => Some(
                    eval(e, n).map_err(|why| error!("bad-group-expression", "{$at}: {$why}", at = format!("{at}.id_base"), why = why))? + o,
                ),
            };
            if let Some(id) = id {
                if !(0..i64::from(RESERVED_BASE)).contains(&id) {
                    return Err(error!(
                        "bad-param-id",
                        "{$at} must be an integer from 0 to {$max} (IDs from {$base} up are Ferritron's)",
                        at = format!("{at} ({key})"),
                        max = RESERVED_BASE - 1,
                        base = format!("0x{RESERVED_BASE:08X}")
                    ));
                }
                p.id = id as u32;
                p.explicit_id = true;
            }
            p.group = Some(GroupRef { group: group_index, instance, field: f });
            if instance == 0 {
                field_keys.push(field.clone());
            }
            params.push(p);
        }
    }
    groups.push(Group { name: name.to_string(), repeat, start, first, fields: field_keys });
    Ok(())
}

fn is_ident(key: &str) -> bool {
    key.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_') && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Replaces `{n}` and `{n:0W}` (zero padded to width W).
fn expand(template: &str, n: i64, at: &str) -> Result<String, Error> {
    let bad = || error!("bad-group-template", "{$at}: only the placeholders n and n:02 (zero padded, width 1 to 9) are allowed, written between curly braces", at = at);
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        out.push_str(&rest[..open]);
        if rest.as_bytes()[open] == b'}' {
            return Err(bad());
        }
        let close = rest[open..].find('}').ok_or_else(bad)? + open;
        match &rest[open + 1..close] {
            "n" => out.push_str(&n.to_string()),
            spec => {
                let width = spec.strip_prefix("n:0").and_then(|w| w.parse::<usize>().ok()).filter(|w| (1..=9).contains(w)).ok_or_else(bad)?;
                out.push_str(&format!("{n:0width$}"));
            }
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A tiny integer expression: `n`, numbers, `+ - *`, parentheses, unary minus.
fn eval(text: &str, n: i64) -> Result<i64, String> {
    struct P<'a> {
        s: &'a [u8],
        i: usize,
        n: i64,
    }
    impl P<'_> {
        fn ws(&mut self) {
            while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
                self.i += 1;
            }
        }
        fn peek(&mut self) -> Option<u8> {
            self.ws();
            self.s.get(self.i).copied()
        }
        fn expr(&mut self) -> Result<i64, String> {
            let mut v = self.term()?;
            while let Some(op @ (b'+' | b'-')) = self.peek() {
                self.i += 1;
                let r = self.term()?;
                v = if op == b'+' { v.checked_add(r) } else { v.checked_sub(r) }.ok_or("the number is too large")?;
            }
            Ok(v)
        }
        fn term(&mut self) -> Result<i64, String> {
            let mut v = self.atom()?;
            while self.peek() == Some(b'*') {
                self.i += 1;
                v = v.checked_mul(self.atom()?).ok_or("the number is too large")?;
            }
            Ok(v)
        }
        fn atom(&mut self) -> Result<i64, String> {
            match self.peek() {
                Some(b'-') => {
                    self.i += 1;
                    Ok(-self.atom()?)
                }
                Some(b'(') => {
                    self.i += 1;
                    let v = self.expr()?;
                    if self.peek() != Some(b')') {
                        return Err("a `)` is missing".into());
                    }
                    self.i += 1;
                    Ok(v)
                }
                Some(b'n') => {
                    self.i += 1;
                    Ok(self.n)
                }
                Some(c) if c.is_ascii_digit() => {
                    let start = self.i;
                    while self.s.get(self.i).is_some_and(u8::is_ascii_digit) {
                        self.i += 1;
                    }
                    std::str::from_utf8(&self.s[start..self.i]).unwrap().parse().map_err(|_| "the number is too large".to_string())
                }
                Some(c) => Err(format!("unexpected `{}` (use numbers, n, + - * and parentheses)", c as char)),
                None => Err("the expression ends too early".into()),
            }
        }
    }
    let mut p = P { s: text.as_bytes(), i: 0, n };
    let v = p.expr()?;
    match p.peek() {
        None => Ok(v),
        Some(c) => Err(format!("unexpected `{}` (use numbers, n, + - * and parentheses)", c as char)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expressions() {
        assert_eq!(eval("2000 + (n-1)*64", 1), Ok(2000));
        assert_eq!(eval("2000 + (n-1)*64", 3), Ok(2128));
        assert_eq!(eval(" -n + 10 ", 4), Ok(6));
        assert_eq!(eval("2*3+4", 0), Ok(10));
        assert!(eval("2 +", 0).is_err());
        assert!(eval("2 / 1", 0).is_err());
        assert!(eval("(2", 0).is_err());
        assert!(eval("m", 0).is_err());
        assert!(eval("99999999999999999999", 0).is_err());
    }

    #[test]
    fn templates() {
        assert_eq!(expand("b{n:02}_", 7, "x").unwrap(), "b07_");
        assert_eq!(expand("band{n}_", 12, "x").unwrap(), "band12_");
        assert_eq!(expand("plain_", 1, "x").unwrap(), "plain_");
        assert_eq!(expand("{n:03}", 5, "x").unwrap(), "005");
        for bad in ["{x}", "{n", "n}", "{n:2}", "{n:00}", "{{n}}"] {
            assert!(expand(bad, 1, "x").is_err(), "{bad}");
        }
    }
}
