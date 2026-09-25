use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct State {
    pub disabled: Vec<String>,
    pub settings: BTreeMap<(String, String), Option<String>>,
}

fn hex(s: &str) -> String {
    s.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::new();
    for i in (0..s.len()).step_by(2) {
        out.push(u8::from_str_radix(&s[i..i + 2], 16).ok()?);
    }
    String::from_utf8(out).ok()
}

pub fn encode(state: &State) -> String {
    let mut out = String::from("emutrim-state-v1\n");
    for p in &state.disabled {
        out.push_str(&format!("d\t{}\n", hex(p)));
    }
    for ((ns, key), value) in &state.settings {
        out.push_str(&format!(
            "s\t{}\t{}\t{}\n",
            hex(ns),
            hex(key),
            value.as_deref().map(hex).unwrap_or_else(|| "-".into())
        ));
    }
    out
}
pub fn decode(text: &str) -> Option<State> {
    let mut lines = text.lines();
    if lines.next()? != "emutrim-state-v1" {
        return None;
    }
    let mut state = State::default();
    for line in lines {
        let p: Vec<_> = line.split('\t').collect();
        match p.as_slice() {
            ["d", value] => state.disabled.push(unhex(value)?),
            ["s", ns, key, value] => {
                state.settings.insert(
                    (unhex(ns)?, unhex(key)?),
                    if *value == "-" {
                        None
                    } else {
                        Some(unhex(value)?)
                    },
                );
            }
            _ => return None,
        };
    }
    Some(state)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_round_trips() {
        let mut state = State::default();
        state.disabled.push("com.example.a".into());
        state
            .settings
            .insert(("global".into(), "x".into()), Some("a b".into()));
        assert_eq!(decode(&encode(&state)), Some(state));
    }
}
