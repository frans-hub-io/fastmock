//! POP3 command lines (RFC 1939, RFC 2449 CAPA, RFC 2595 STLS, RFC 5034 AUTH): keywords in any case,
//! arguments separated by spaces, message numbers as decimal digits.

/// A parsed command.
#[derive(Clone, PartialEq, Eq)]
pub enum Command {
    User(String),
    /// The rest of the line after `PASS ` (it may contain spaces).
    Pass(String),
    /// `AUTH` alone lists the mechanisms.
    Auth {
        mechanism: Option<String>,
        initial: Option<String>,
    },
    Apop,
    Capa,
    Stls,
    Quit,
    Stat,
    List(Option<u32>),
    Retr(u32),
    Dele(u32),
    Noop,
    Rset,
    Top(u32, u32),
    Uidl(Option<u32>),
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::Pass(_) => f.write_str("Pass(****)"),
            Command::Auth { mechanism, initial } => f
                .debug_struct("Auth")
                .field("mechanism", mechanism)
                .field("initial", &initial.as_ref().map(|_| "****"))
                .finish(),
            Command::User(u) => f.debug_tuple("User").field(u).finish(),
            Command::Apop => f.write_str("Apop"),
            Command::Capa => f.write_str("Capa"),
            Command::Stls => f.write_str("Stls"),
            Command::Quit => f.write_str("Quit"),
            Command::Stat => f.write_str("Stat"),
            Command::List(n) => f.debug_tuple("List").field(n).finish(),
            Command::Retr(n) => f.debug_tuple("Retr").field(n).finish(),
            Command::Dele(n) => f.debug_tuple("Dele").field(n).finish(),
            Command::Noop => f.write_str("Noop"),
            Command::Rset => f.write_str("Rset"),
            Command::Top(n, l) => f.debug_tuple("Top").field(n).field(l).finish(),
            Command::Uidl(n) => f.debug_tuple("Uidl").field(n).finish(),
        }
    }
}

impl Command {
    /// The keyword, upper case.
    pub fn name(&self) -> &'static str {
        match self {
            Command::User(_) => "USER",
            Command::Pass(_) => "PASS",
            Command::Auth { .. } => "AUTH",
            Command::Apop => "APOP",
            Command::Capa => "CAPA",
            Command::Stls => "STLS",
            Command::Quit => "QUIT",
            Command::Stat => "STAT",
            Command::List(_) => "LIST",
            Command::Retr(_) => "RETR",
            Command::Dele(_) => "DELE",
            Command::Noop => "NOOP",
            Command::Rset => "RSET",
            Command::Top(..) => "TOP",
            Command::Uidl(_) => "UIDL",
        }
    }
}

/// Why a line is not a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// Not UTF-8, or control characters.
    BadCharacters,
    Unknown,
    /// Wrong arguments; the text shows the expected form.
    Syntax(&'static str),
    /// A message number or line count that is not a decimal number.
    BadNumber,
}

impl CommandError {
    /// The `-ERR` text.
    pub fn text(&self) -> String {
        match self {
            CommandError::BadCharacters => "Invalid characters in command".into(),
            CommandError::Unknown => "Unknown command".into(),
            CommandError::Syntax(s) => format!("Syntax: {s}"),
            CommandError::BadNumber => "Invalid message number".into(),
        }
    }
}

fn number(s: &str) -> Result<u32, CommandError> {
    if s.is_empty() || s.len() > 10 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(CommandError::BadNumber);
    }
    s.parse().map_err(|_| CommandError::BadNumber)
}

/// The line without its line break.
pub fn strip_eol(line: &[u8]) -> &[u8] {
    let l = line.strip_suffix(b"\n").unwrap_or(line);
    l.strip_suffix(b"\r").unwrap_or(l)
}

/// Parses one command line (with or without its line break).
pub fn parse(line: &[u8]) -> Result<Command, CommandError> {
    let text = std::str::from_utf8(strip_eol(line)).map_err(|_| CommandError::BadCharacters)?;
    if text.chars().any(|c| c.is_control() && c != '\t') {
        return Err(CommandError::BadCharacters);
    }
    let (kw, rest) = match text.split_once(' ') {
        Some((k, r)) => (k, Some(r)),
        None => (text, None),
    };
    let kw = kw.to_ascii_uppercase();
    if kw == "PASS" {
        return match rest {
            Some(p) if !p.is_empty() => Ok(Command::Pass(p.to_string())),
            _ => Err(CommandError::Syntax("PASS <password>")),
        };
    }
    let args: Vec<&str> = rest.map_or_else(Vec::new, |r| r.split_ascii_whitespace().collect());
    let none = |c: Command, form: &'static str| {
        if args.is_empty() {
            Ok(c)
        } else {
            Err(CommandError::Syntax(form))
        }
    };
    match kw.as_str() {
        "USER" => match args.as_slice() {
            [u] => Ok(Command::User(u.to_string())),
            _ => Err(CommandError::Syntax("USER <mailbox address>")),
        },
        "AUTH" => match args.as_slice() {
            [] => Ok(Command::Auth {
                mechanism: None,
                initial: None,
            }),
            [m] => Ok(Command::Auth {
                mechanism: Some(m.to_ascii_uppercase()),
                initial: None,
            }),
            [m, i] => Ok(Command::Auth {
                mechanism: Some(m.to_ascii_uppercase()),
                initial: Some(i.to_string()),
            }),
            _ => Err(CommandError::Syntax("AUTH <mechanism> [initial-response]")),
        },
        "APOP" => Ok(Command::Apop),
        "CAPA" => none(Command::Capa, "CAPA"),
        "STLS" => none(Command::Stls, "STLS"),
        "QUIT" => none(Command::Quit, "QUIT"),
        "STAT" => none(Command::Stat, "STAT"),
        "NOOP" => none(Command::Noop, "NOOP"),
        "RSET" => none(Command::Rset, "RSET"),
        "LIST" => match args.as_slice() {
            [] => Ok(Command::List(None)),
            [n] => Ok(Command::List(Some(number(n)?))),
            _ => Err(CommandError::Syntax("LIST [msg]")),
        },
        "UIDL" => match args.as_slice() {
            [] => Ok(Command::Uidl(None)),
            [n] => Ok(Command::Uidl(Some(number(n)?))),
            _ => Err(CommandError::Syntax("UIDL [msg]")),
        },
        "RETR" => match args.as_slice() {
            [n] => Ok(Command::Retr(number(n)?)),
            _ => Err(CommandError::Syntax("RETR <msg>")),
        },
        "DELE" => match args.as_slice() {
            [n] => Ok(Command::Dele(number(n)?)),
            _ => Err(CommandError::Syntax("DELE <msg>")),
        },
        "TOP" => match args.as_slice() {
            [n, l] => Ok(Command::Top(number(n)?, number(l)?)),
            _ => Err(CommandError::Syntax("TOP <msg> <lines>")),
        },
        _ => Err(CommandError::Unknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(
            parse(b"user Alice@example.test\r\n"),
            Ok(Command::User("Alice@example.test".into()))
        );
        assert_eq!(
            parse(b"PASS two words \r\n"),
            Ok(Command::Pass("two words ".into()))
        );
        assert_eq!(
            parse(b"PASS\r\n"),
            Err(CommandError::Syntax("PASS <password>"))
        );
        assert_eq!(
            parse(b"PASS \r\n"),
            Err(CommandError::Syntax("PASS <password>"))
        );
        assert_eq!(
            parse(b"auth plain AGEAYg==\r\n"),
            Ok(Command::Auth {
                mechanism: Some("PLAIN".into()),
                initial: Some("AGEAYg==".into())
            })
        );
        assert_eq!(
            parse(b"AUTH\n"),
            Ok(Command::Auth {
                mechanism: None,
                initial: None
            })
        );
        assert!(parse(b"AUTH a b c").is_err());
        assert_eq!(parse(b"capa"), Ok(Command::Capa));
        assert_eq!(parse(b"STAT x"), Err(CommandError::Syntax("STAT")));
        assert_eq!(parse(b"LIST\r\n"), Ok(Command::List(None)));
        assert_eq!(parse(b"LIST 2\r\n"), Ok(Command::List(Some(2))));
        assert_eq!(parse(b"LIST -1\r\n"), Err(CommandError::BadNumber));
        assert_eq!(parse(b"LIST 99999999999\r\n"), Err(CommandError::BadNumber));
        assert_eq!(parse(b"LIST 4294967296\r\n"), Err(CommandError::BadNumber));
        assert_eq!(
            parse(b"LIST 1 2\r\n"),
            Err(CommandError::Syntax("LIST [msg]"))
        );
        assert_eq!(parse(b"uidl 7"), Ok(Command::Uidl(Some(7))));
        assert_eq!(parse(b"RETR 1"), Ok(Command::Retr(1)));
        assert_eq!(parse(b"RETR"), Err(CommandError::Syntax("RETR <msg>")));
        assert_eq!(parse(b"DELE +1"), Err(CommandError::BadNumber));
        assert_eq!(parse(b"TOP 1 0"), Ok(Command::Top(1, 0)));
        assert_eq!(parse(b"TOP 1 -1"), Err(CommandError::BadNumber));
        assert_eq!(
            parse(b"TOP 1"),
            Err(CommandError::Syntax("TOP <msg> <lines>"))
        );
        assert_eq!(parse(b"APOP x y"), Ok(Command::Apop));
        assert_eq!(parse(b"XTND"), Err(CommandError::Unknown));
        assert_eq!(parse(b""), Err(CommandError::Unknown));
        assert_eq!(parse(b"\xff\xfe"), Err(CommandError::BadCharacters));
        assert_eq!(parse(b"USER a\x00b"), Err(CommandError::BadCharacters));
        assert!(!format!("{:?}", parse(b"PASS secret").unwrap()).contains("secret"));
        let auth = parse(b"AUTH PLAIN c2VjcmV0").unwrap();
        assert!(!format!("{auth:?}").contains("c2VjcmV0"));
        assert_eq!(auth.name(), "AUTH");
    }
}
