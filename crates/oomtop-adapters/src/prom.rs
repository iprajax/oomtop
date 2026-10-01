//! Minimal Prometheus text-format parser (exposition format 0.0.4) for llama.cpp and vLLM `/metrics`.

/// One sample line: `name{label="v",…} value [timestamp]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Sample {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Parsed metrics page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metrics {
    pub samples: Vec<Sample>,
}

impl Metrics {
    pub fn parse(text: &str) -> Metrics {
        Metrics {
            samples: text.lines().filter_map(parse_line).collect(),
        }
    }

    /// All samples named `name`.
    pub fn get<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Sample> + 'a {
        self.samples.iter().filter(move |s| s.name == name)
    }

    /// Sum of all finite samples named `name` (across label sets); `None` if absent.
    pub fn sum(&self, name: &str) -> Option<f64> {
        let mut any = false;
        let mut total = 0.0;
        for s in self.get(name).filter(|s| s.value.is_finite()) {
            any = true;
            total += s.value;
        }
        any.then_some(total)
    }

    /// First of `names` that is present, summed.
    pub fn sum_any(&self, names: &[&str]) -> Option<f64> {
        names.iter().find_map(|n| self.sum(n))
    }

    /// Maximum finite value of `name`.
    pub fn max(&self, name: &str) -> Option<f64> {
        self.get(name)
            .map(|s| s.value)
            .filter(|v| v.is_finite())
            .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))))
    }

    /// Distinct values of label `key` across all samples of the given names.
    pub fn label_values(&self, names: &[&str], key: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in self.samples.iter().filter(|s| names.contains(&s.name.as_str())) {
            if let Some(v) = s.label(key) {
                if !v.is_empty() && !out.iter().any(|o| o == v) {
                    out.push(v.to_string());
                }
            }
        }
        out
    }
}

fn parse_value(s: &str) -> Option<f64> {
    match s {
        "+Inf" | "Inf" => Some(f64::INFINITY),
        "-Inf" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ => s.parse().ok(),
    }
}

fn parse_line(line: &str) -> Option<Sample> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let i = line.find(['{', ' ', '\t'])?;
    let (name, rest) = (&line[..i], &line[i..]);
    if name.is_empty() {
        return None;
    }
    let (labels, rest) = if let Some(r) = rest.strip_prefix('{') {
        parse_labels(r)?
    } else {
        (Vec::new(), rest)
    };
    let value = parse_value(rest.split_whitespace().next()?)?;
    Some(Sample {
        name: name.to_string(),
        labels,
        value,
    })
}

/// Parses `a="x",b="y\"z"}` and returns the labels plus the text after `}`.
fn parse_labels(s: &str) -> Option<(Vec<(String, String)>, &str)> {
    let mut labels = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    loop {
        while i < b.len() && (b[i] == b' ' || b[i] == b',') {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        if b[i] == b'}' {
            return Some((labels, &s[i + 1..]));
        }
        let key_start = i;
        while i < b.len() && b[i] != b'=' {
            i += 1;
        }
        let key = s[key_start..i].trim().to_string();
        i += 1; // '='
        if i >= b.len() || b[i] != b'"' {
            return None;
        }
        i += 1;
        let mut val = String::new();
        loop {
            let c = *b.get(i)?;
            match c {
                b'\\' => {
                    // The escaped character may be multi-byte: step over it by char, never by byte.
                    let n = s.get(i + 1..)?.chars().next()?;
                    val.push(if n == 'n' { '\n' } else { n });
                    i += 1 + n.len_utf8();
                }
                b'"' => {
                    i += 1;
                    break;
                }
                _ => {
                    // copy one UTF-8 character
                    let ch = s[i..].chars().next()?;
                    val.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        labels.push((key, val));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_llama_cpp_and_vllm_pages() {
        let text = r#"
# HELP llamacpp:prompt_tokens_total Number of prompt tokens processed.
# TYPE llamacpp:prompt_tokens_total counter
llamacpp:prompt_tokens_total 1234
llamacpp:predicted_tokens_seconds 38.5
llamacpp:requests_processing 1
llamacpp:requests_deferred 2
vllm:num_requests_running{engine="0",model_name="Qwen/Qwen3-8B"} 3.0
vllm:num_requests_running{engine="1",model_name="Qwen/Qwen3-8B"} 1.0
vllm:kv_cache_usage_perc{model_name="Qwen/Qwen3-8B"} 0.25
weird{a="x\"y",b="line\nbreak",c="ü"} NaN 1700000000
inf_metric +Inf
bad{a=unquoted} 1
"#;
        let m = Metrics::parse(text);
        assert_eq!(m.sum("llamacpp:prompt_tokens_total"), Some(1234.0));
        assert_eq!(m.sum("llamacpp:predicted_tokens_seconds"), Some(38.5));
        assert_eq!(m.sum("vllm:num_requests_running"), Some(4.0));
        assert_eq!(m.max("vllm:num_requests_running"), Some(3.0));
        assert_eq!(m.sum("absent"), None);
        assert_eq!(
            m.sum_any(&["vllm:gpu_cache_usage_perc", "vllm:kv_cache_usage_perc"]),
            Some(0.25)
        );
        assert_eq!(
            m.label_values(&["vllm:num_requests_running"], "model_name"),
            vec!["Qwen/Qwen3-8B".to_string()]
        );
        let w = m.get("weird").next().unwrap();
        assert_eq!(w.label("a"), Some("x\"y"));
        assert_eq!(w.label("b"), Some("line\nbreak"));
        assert_eq!(w.label("c"), Some("ü"));
        assert!(w.value.is_nan());
        assert_eq!(m.sum("weird"), None, "NaN is not summed");
        assert!(m.get("bad").next().is_none());
        assert_eq!(m.get("inf_metric").next().unwrap().value, f64::INFINITY);
    }

    /// A backslash before a multi-byte character used to slice inside the character and panic.
    #[test]
    fn escapes_never_split_utf8() {
        let m = Metrics::parse("x{a=\"\\ü\",b=\"\\\\\"} 1\ny{a=\"\\\"} 2\nz{a=\"\\");
        let x = m.get("x").next().unwrap();
        assert_eq!(x.label("a"), Some("ü"));
        assert_eq!(x.label("b"), Some("\\"));
        assert!(m.get("y").next().is_none(), "unterminated value is skipped");
        assert!(m.get("z").next().is_none());
    }
}
