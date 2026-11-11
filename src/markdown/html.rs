//! A tiny, forgiving HTML tokenizer — enough for the HTML commonly found in
//! READMEs (`<img>`, `<p align="center">`, `<br>`, `<a>`, `<b>`, `<h1>`, ...).

#[derive(Debug, PartialEq)]
pub enum Token {
    Open {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    Close(String),
    Text(String),
}

impl Token {
    pub fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
        attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

pub fn tokenize(s: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    let mut text_start = 0;
    while i < b.len() {
        if b[i] == b'<' {
            if s[i..].starts_with("<!--") {
                push_text(&mut out, &s[text_start..i]);
                let end = s[i + 4..]
                    .find("-->")
                    .map(|e| i + 4 + e + 3)
                    .unwrap_or(b.len());
                i = end;
                text_start = i;
                continue;
            }
            if let Some((tok, len)) = parse_tag(&s[i..]) {
                push_text(&mut out, &s[text_start..i]);
                out.push(tok);
                i += len;
                text_start = i;
                continue;
            }
        }
        i += 1;
    }
    push_text(&mut out, &s[text_start..]);
    out
}

fn push_text(out: &mut Vec<Token>, t: &str) {
    if !t.is_empty() {
        out.push(Token::Text(decode_entities(t)));
    }
}

fn parse_tag(s: &str) -> Option<(Token, usize)> {
    let end = find_tag_end(s)?;
    let inner = &s[1..end];
    if let Some(name) = inner.strip_prefix('/') {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        return Some((Token::Close(name), end + 1));
    }
    let self_closing = inner.trim_end().ends_with('/');
    let inner = inner.trim_end().trim_end_matches('/');
    let name_end = inner
        .find(|c: char| c.is_whitespace())
        .unwrap_or(inner.len());
    let name = inner[..name_end].to_ascii_lowercase();
    if name.is_empty()
        || !name.chars().next()?.is_ascii_alphabetic()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let attrs = parse_attrs(&inner[name_end..]);
    Some((
        Token::Open {
            name,
            attrs,
            self_closing,
        },
        end + 1,
    ))
}

fn find_tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices().skip(1) {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(i),
            None if c == '<' => return None,
            None => {}
        }
    }
    None
}

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < c.len() {
        while i < c.len() && c[i].is_whitespace() {
            i += 1;
        }
        let ks = i;
        while i < c.len() && !c[i].is_whitespace() && c[i] != '=' {
            i += 1;
        }
        let key: String = c[ks..i].iter().collect::<String>().to_ascii_lowercase();
        while i < c.len() && c[i].is_whitespace() {
            i += 1;
        }
        let mut val = String::new();
        if i < c.len() && c[i] == '=' {
            i += 1;
            while i < c.len() && c[i].is_whitespace() {
                i += 1;
            }
            if i < c.len() && (c[i] == '"' || c[i] == '\'') {
                let q = c[i];
                i += 1;
                let vs = i;
                while i < c.len() && c[i] != q {
                    i += 1;
                }
                val = c[vs..i].iter().collect();
                i += 1;
            } else {
                let vs = i;
                while i < c.len() && !c[i].is_whitespace() {
                    i += 1;
                }
                val = c[vs..i].iter().collect();
            }
        }
        if !key.is_empty() {
            out.push((key, decode_entities(&val)));
        }
    }
    out
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(end) = after.find(';').filter(|&e| e <= 10) {
            let ent = &after[..end];
            let rep = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                "copy" => Some('©'),
                "reg" => Some('®'),
                "trade" => Some('™'),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "hellip" => Some('…'),
                "middot" => Some('·'),
                _ => ent
                    .strip_prefix("#x")
                    .or_else(|| ent.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            };
            if let Some(c) = rep {
                out.push(c);
                rest = &after[end + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_readme_html() {
        let t = tokenize(
            r#"<p align="center"><img src="logo.png" width=200 alt='Logo'/></p><!-- c -->a &amp; b<br>"#,
        );
        assert_eq!(t.len(), 5);
        match &t[1] {
            Token::Open {
                name,
                attrs,
                self_closing,
            } => {
                assert_eq!(name, "img");
                assert!(self_closing);
                assert_eq!(Token::attr(attrs, "src"), Some("logo.png"));
                assert_eq!(Token::attr(attrs, "width"), Some("200"));
                assert_eq!(Token::attr(attrs, "alt"), Some("Logo"));
            }
            _ => panic!(),
        }
        assert_eq!(t[3], Token::Text("a & b".into()));
        assert!(matches!(&t[4], Token::Open { name, .. } if name == "br"));
        assert_eq!(tokenize("a < b").len(), 1);
    }
}
