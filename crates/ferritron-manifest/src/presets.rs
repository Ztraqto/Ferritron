//! `[presets.<id>]`: factory presets (N4).
//!
//! ```toml
//! [presets.warm]
//! name   = "Warm Pad"     # optional; the id is shown otherwise
//! cutoff = 800            # a parameter key and its plain value
//! mode   = "Dark"         # enums: the choice's name (or its position)
//! on     = true
//! ```
//!
//! Every key but `name` is a parameter of the plugin (`name` too, when there is a parameter of
//! that name). Values must be inside the parameter's range. The parameters a preset does not
//! list keep their value when it is loaded.

use super::*;

/// One factory preset.
#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    /// The table's key.
    pub id: String,
    /// `name = "..."`, else the id.
    pub name: String,
    /// `(index in the project file's parameter list, plain value)`, in the order written.
    pub values: Vec<(usize, f64)>,
}

pub(crate) fn parse_presets(root: &Table, params: &[Param]) -> Result<Vec<Preset>, Error> {
    let Some(v) = root.get("presets") else { return Ok(Vec::new()) };
    let table = v.as_table().ok_or_else(|| not_table("[presets]"))?;
    let mut out = Vec::new();
    for (id, value) in table {
        let at = format!("presets.{id}");
        let t = value.as_table().ok_or_else(|| not_table(&at))?;
        let mut name = id.clone();
        let mut values = Vec::new();
        for (key, v) in t {
            let found = params.iter().position(|p| &p.key == key);
            let Some(index) = found else {
                if key == "name" {
                    name = v.as_str().ok_or_else(|| not_string(&format!("{at}.name")))?.to_string();
                    continue;
                }
                return Err(error!("preset-unknown-param", "{$at} is not a parameter of the plugin", at = format!("{at}.{key}")));
            };
            values.push((index, preset_value(&params[index], v, &format!("{at}.{key}"))?));
        }
        out.push(Preset { id: id.clone(), name, values });
    }
    Ok(out)
}

fn preset_value(p: &Param, v: &Value, at: &str) -> Result<f64, Error> {
    let bad = || error!("preset-bad-value", "{$at} is not a valid value for this parameter", at = at);
    let out_of_range = |min: f64, max: f64| error!("preset-out-of-range", "{$at} must be from {$min} to {$max}", at = at, min = min, max = max);
    match &p.kind {
        Kind::Float { min, max } => {
            let x = v.as_float_lossy().ok_or_else(bad)?;
            (*min..=*max).contains(&x).then_some(x).ok_or_else(|| out_of_range(*min, *max))
        }
        Kind::Int { min, max } => {
            let x = v.as_integer().ok_or_else(bad)?;
            (*min..=*max).contains(&x).then_some(x as f64).ok_or_else(|| out_of_range(*min as f64, *max as f64))
        }
        Kind::Bool => v.as_bool().map(|b| if b { 1.0 } else { 0.0 }).ok_or_else(bad),
        Kind::Enum { choices } => match v {
            Value::String(s) => choices.iter().position(|c| c.eq_ignore_ascii_case(s)).map(|i| i as f64).ok_or_else(bad),
            Value::Integer(n) if (0..choices.len() as i64).contains(n) => Ok(*n as f64),
            _ => Err(bad()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(presets: &str) -> Result<Manifest, Error> {
        parse(&format!(
            "[plugin]\nid = \"a.b\"\nname = \"X\"\n[params]\ncutoff = {{ type = \"float\", range = [20, 20000], default = 1000 }}\nvoices = {{ type = \"int\", range = [1, 8] }}\non = {{ type = \"bool\" }}\nmode = {{ type = \"enum\", choices = [\"Clean\", \"Dark\"] }}\n{presets}"
        ))
    }

    #[test]
    fn presets_list_values_by_parameter_position() {
        let m = project("[presets.warm]\nname = \"Warm Pad\"\ncutoff = 800\nvoices = 4\non = true\nmode = \"dark\"\n[presets.init]\ncutoff = 1000.5").unwrap();
        assert_eq!(m.presets.len(), 2);
        let warm = m.presets.iter().find(|p| p.id == "warm").unwrap();
        assert_eq!(warm.name, "Warm Pad");
        let mut v = warm.values.clone();
        v.sort_by_key(|x| x.0);
        assert_eq!(v, vec![(0, 800.0), (1, 4.0), (2, 1.0), (3, 1.0)]);
        let init = m.presets.iter().find(|p| p.id == "init").unwrap();
        assert_eq!((init.name.as_str(), init.values.clone()), ("init", vec![(0, 1000.5)]));
        assert!(project("").unwrap().presets.is_empty());
    }

    #[test]
    fn mistakes_have_codes() {
        let code = |s: &str| project(s).err().map(|e| e.code);
        assert_eq!(code("[presets.a]\nnope = 1"), Some("preset-unknown-param".into()));
        assert_eq!(code("[presets.a]\ncutoff = 5"), Some("preset-out-of-range".into()));
        assert_eq!(code("[presets.a]\nvoices = 9"), Some("preset-out-of-range".into()));
        assert_eq!(code("[presets.a]\ncutoff = \"high\""), Some("preset-bad-value".into()));
        assert_eq!(code("[presets.a]\nvoices = 2.5"), Some("preset-bad-value".into()));
        assert_eq!(code("[presets.a]\nmode = \"Wet\""), Some("preset-bad-value".into()));
        assert_eq!(code("[presets.a]\nmode = 2"), Some("preset-bad-value".into()));
        assert_eq!(code("[presets.a]\non = 1"), Some("preset-bad-value".into()));
        assert_eq!(code("[presets]\na = 1"), Some("not-table".into()));
    }
}
