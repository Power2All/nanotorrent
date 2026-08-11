// Port of src/picotorrent/ui/filters/{torrentfilter,pqltorrentfilter}.{hpp,cpp}
//
// Implements the subset of PicoTorrent Query Language (PQL) needed for the
// built-in filters, e.g.:
//
//     status = "downloading" and dl > 1kbps
//     name contains "linux" or size >= 1gb

use crate::bittorrent::torrentstatus::{State, TorrentStatus};

#[derive(Debug, Clone)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp {
        field: String,
        op: Op,
        value: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Lt,
    Gte,
    Lte,
    Contains,
}

#[derive(Debug, Clone)]
pub enum Value {
    Str(String),
    Num(f64),
}

pub struct TorrentFilter {
    expr: Expr,
}

impl TorrentFilter {
    pub fn parse(input: &str) -> Result<TorrentFilter, String> {
        let tokens = tokenize(input)?;
        let mut pos = 0usize;
        let expr = parse_or(&tokens, &mut pos)?;
        if pos != tokens.len() {
            return Err(format!("unexpected token at position {pos}"));
        }
        Ok(TorrentFilter { expr })
    }

    pub fn includes(&self, status: &TorrentStatus) -> bool {
        eval(&self.expr, status)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Num(f64),
    Op(Op),
    And,
    Or,
    LParen,
    RParen,
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '"' => {
                let mut s = String::new();
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    s.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(String::from("unterminated string"));
                }
                i += 1;
                tokens.push(Token::Str(s));
            }
            '=' => {
                tokens.push(Token::Op(Op::Eq));
                i += 1;
            }
            '!' if i + 1 < chars.len() && chars[i + 1] == '=' => {
                tokens.push(Token::Op(Op::Ne));
                i += 2;
            }
            '>' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::Op(Op::Gte));
                    i += 2;
                } else {
                    tokens.push(Token::Op(Op::Gt));
                    i += 1;
                }
            }
            '<' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::Op(Op::Lte));
                    i += 2;
                } else {
                    tokens.push(Token::Op(Op::Lt));
                    i += 1;
                }
            }
            c if c.is_ascii_digit() => {
                let mut num = String::new();
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    num.push(chars[i]);
                    i += 1;
                }
                // optional unit suffix
                let mut unit = String::new();
                while i < chars.len() && chars[i].is_ascii_alphabetic() {
                    unit.push(chars[i].to_ascii_lowercase());
                    i += 1;
                }
                let base: f64 = num.parse().map_err(|_| format!("bad number: {num}"))?;
                let mult = match unit.as_str() {
                    "" | "b" | "bps" => 1.0,
                    "kb" | "kbps" => 1024.0,
                    "mb" | "mbps" => 1024.0 * 1024.0,
                    "gb" | "gbps" => 1024.0 * 1024.0 * 1024.0,
                    "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
                    _ => return Err(format!("unknown unit: {unit}")),
                };
                tokens.push(Token::Num(base * mult));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut ident = String::new();
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '.')
                {
                    ident.push(chars[i]);
                    i += 1;
                }
                match ident.to_ascii_lowercase().as_str() {
                    "and" => tokens.push(Token::And),
                    "or" => tokens.push(Token::Or),
                    "contains" => tokens.push(Token::Op(Op::Contains)),
                    _ => tokens.push(Token::Ident(ident.to_ascii_lowercase())),
                }
            }
            other => return Err(format!("unexpected character: {other}")),
        }
    }

    Ok(tokens)
}

fn parse_or(tokens: &[Token], pos: &mut usize) -> Result<Expr, String> {
    let mut left = parse_and(tokens, pos)?;
    while *pos < tokens.len() && tokens[*pos] == Token::Or {
        *pos += 1;
        let right = parse_and(tokens, pos)?;
        left = Expr::Or(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_and(tokens: &[Token], pos: &mut usize) -> Result<Expr, String> {
    let mut left = parse_cmp(tokens, pos)?;
    while *pos < tokens.len() && tokens[*pos] == Token::And {
        *pos += 1;
        let right = parse_cmp(tokens, pos)?;
        left = Expr::And(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_cmp(tokens: &[Token], pos: &mut usize) -> Result<Expr, String> {
    if *pos < tokens.len() && tokens[*pos] == Token::LParen {
        *pos += 1;
        let inner = parse_or(tokens, pos)?;
        if *pos >= tokens.len() || tokens[*pos] != Token::RParen {
            return Err(String::from("expected )"));
        }
        *pos += 1;
        return Ok(inner);
    }

    let Some(Token::Ident(field)) = tokens.get(*pos) else {
        return Err(String::from("expected field name"));
    };
    *pos += 1;

    let Some(Token::Op(op)) = tokens.get(*pos) else {
        return Err(String::from("expected operator"));
    };
    *pos += 1;

    let value = match tokens.get(*pos) {
        Some(Token::Str(s)) => Value::Str(s.clone()),
        Some(Token::Num(n)) => Value::Num(*n),
        Some(Token::Ident(s)) => Value::Str(s.clone()),
        _ => return Err(String::from("expected value")),
    };
    *pos += 1;

    Ok(Expr::Cmp {
        field: field.clone(),
        op: *op,
        value,
    })
}

fn status_name(state: State) -> &'static str {
    match state {
        State::Downloading
        | State::DownloadingChecking
        | State::DownloadingMetadata
        | State::DownloadingQueued => "downloading",
        State::Uploading | State::UploadingQueued => "uploading",
        State::DownloadingPaused | State::UploadingPaused => "paused",
        State::CheckingFiles | State::CheckingResumeData => "checking",
        State::Error => "error",
        State::Unknown => "unknown",
    }
}

fn eval(expr: &Expr, s: &TorrentStatus) -> bool {
    match expr {
        Expr::And(a, b) => eval(a, s) && eval(b, s),
        Expr::Or(a, b) => eval(a, s) || eval(b, s),
        Expr::Cmp { field, op, value } => {
            let num = |v: &Value| match v {
                Value::Num(n) => Some(*n),
                Value::Str(_) => None,
            };

            match field.as_str() {
                "name" => cmp_str(&s.name, *op, value),
                "status" => cmp_str(status_name(s.state), *op, value),
                "label" => cmp_str(&s.label_name, *op, value),
                "dl" => num(value).map(|n| cmp_num(s.download_payload_rate as f64, *op, n)).unwrap_or(false),
                "ul" => num(value).map(|n| cmp_num(s.upload_payload_rate as f64, *op, n)).unwrap_or(false),
                "size" => num(value).map(|n| cmp_num(s.total_wanted as f64, *op, n)).unwrap_or(false),
                "progress" => num(value).map(|n| cmp_num(s.progress as f64 * 100.0, *op, n)).unwrap_or(false),
                "ratio" => num(value).map(|n| cmp_num(s.ratio as f64, *op, n)).unwrap_or(false),
                _ => false,
            }
        }
    }
}

fn cmp_str(actual: &str, op: Op, value: &Value) -> bool {
    let Value::Str(expected) = value else {
        return false;
    };
    let actual_lc = actual.to_lowercase();
    let expected_lc = expected.to_lowercase();
    match op {
        Op::Eq => actual_lc == expected_lc,
        Op::Ne => actual_lc != expected_lc,
        Op::Contains => actual_lc.contains(&expected_lc),
        _ => false,
    }
}

fn cmp_num(actual: f64, op: Op, expected: f64) -> bool {
    match op {
        Op::Eq => actual == expected,
        Op::Ne => actual != expected,
        Op::Gt => actual > expected,
        Op::Lt => actual < expected,
        Op::Gte => actual >= expected,
        Op::Lte => actual <= expected,
        Op::Contains => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    fn status(name: &str, state: State) -> TorrentStatus {
        TorrentStatus {
            added_on: Local::now(),
            all_time_download: 0,
            all_time_upload: 0,
            availability: 0.0,
            completed_on: None,
            download_payload_rate: 0,
            error: String::new(),
            eta: None,
            info_hash: String::new(),
            label_id: None,
            label_name: String::new(),
            name: name.into(),
            paused: false,
            peers_current: 0,
            peers_total: 0,
            progress: 0.0,
            queue_position: 0,
            ratio: 0.0,
            save_path: String::new(),
            seeds_current: 0,
            seeds_total: 0,
            state,
            total_wanted: 0,
            total_wanted_remaining: 0,
            upload_payload_rate: 0,
        }
    }

    fn matches(query: &str, s: &TorrentStatus) -> bool {
        TorrentFilter::parse(query).unwrap().includes(s)
    }

    #[test]
    fn name_contains_and_status_eq() {
        let mut s = status("Ubuntu 24.04 ISO", State::Downloading);
        assert!(matches(r#"name contains "ubuntu""#, &s)); // case-insensitive
        assert!(!matches(r#"name contains "debian""#, &s));
        assert!(matches(r#"status = "downloading""#, &s));
        s.state = State::UploadingPaused;
        assert!(matches(r#"status = "paused""#, &s));
        assert!(matches(r#"status != "downloading""#, &s));
    }

    #[test]
    fn numeric_units_and_comparisons() {
        let mut s = status("x", State::Downloading);
        s.download_payload_rate = 2 * 1024 * 1024; // 2 MB/s
        s.total_wanted = 3 * 1024i64.pow(3); // 3 GB
        assert!(matches("dl > 1mbps", &s));
        assert!(!matches("dl > 5mbps", &s));
        assert!(matches("size >= 3gb", &s));
        assert!(matches("size < 4gb", &s));
        // bare number = bytes
        s.download_payload_rate = 500;
        assert!(matches("dl < 1024", &s));
    }

    #[test]
    fn and_or_precedence_and_parens() {
        let s = status("linux", State::Downloading);
        // OR binds looser than AND: this is (status=downloading AND name contains linux) OR name contains bsd
        assert!(matches(
            r#"status = "downloading" and name contains "linux" or name contains "bsd""#,
            &s
        ));
        assert!(matches(r#"(status = "seeding" or status = "downloading")"#, &s));
        assert!(!matches(r#"status = "downloading" and name contains "bsd""#, &s));
    }

    #[test]
    fn progress_is_percent_and_ratio() {
        let mut s = status("x", State::Downloading);
        s.progress = 0.5;
        s.ratio = 1.5;
        assert!(matches("progress >= 50", &s));
        assert!(!matches("progress > 50", &s));
        assert!(matches("ratio > 1", &s));
    }

    #[test]
    fn parse_errors() {
        assert!(TorrentFilter::parse(r#"name contains"#).is_err()); // missing value
        assert!(TorrentFilter::parse(r#"name "linux""#).is_err()); // missing operator
        assert!(TorrentFilter::parse(r#""unterminated"#).is_err());
        assert!(TorrentFilter::parse(r#"dl > 5xb"#).is_err()); // unknown unit
        assert!(TorrentFilter::parse(r#"(status = "downloading""#).is_err()); // unbalanced paren
    }
}
