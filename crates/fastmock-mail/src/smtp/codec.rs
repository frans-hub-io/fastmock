//! The SMTP wire format behind an internal trait: command lines and the DATA body decoder. The session
//! only sees [`Command`], [`CommandError`] and [`BodyDecoder`], so `smtp-proto` can be swapped for a
//! hand-written parser without touching the state machine.

/// Longest accepted e-mail address in MAIL and RCPT (the `smtp-proto` limit).
pub const MAX_ADDRESS_LEN: usize = 256;

/// SASL mechanisms the server knows by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mechanism {
    Plain,
    Login,
    CramMd5,
    /// Any other mechanism name.
    Other,
}

impl Mechanism {
    pub fn as_str(self) -> &'static str {
        match self {
            Mechanism::Plain => "PLAIN",
            Mechanism::Login => "LOGIN",
            Mechanism::CramMd5 => "CRAM-MD5",
            Mechanism::Other => "other",
        }
    }
}

/// `BODY=` of MAIL FROM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyType {
    SevenBit,
    EightBitMime,
    BinaryMime,
}

/// A parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Ehlo {
        host: String,
    },
    Helo {
        host: String,
    },
    Mail {
        /// Empty for the null sender `<>`.
        address: String,
        /// `SIZE=`, when given.
        size: Option<usize>,
        smtputf8: bool,
        body: BodyType,
    },
    Rcpt {
        /// `postmaster` for `<postmaster>`.
        address: String,
    },
    Data,
    Bdat {
        size: usize,
        last: bool,
    },
    Auth {
        mechanism: Mechanism,
        /// The initial response as sent (base64, or `=` for an empty one).
        initial: Option<String>,
    },
    StartTls,
    Rset,
    Noop,
    Vrfy,
    Quit,
    /// A known SMTP command this server does not implement (EXPN, HELP, ETRN, ATRN, BURL, LHLO).
    NotImplemented(&'static str),
}

/// Why a command line was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    Unknown,
    BadSender,
    BadRecipient,
    Syntax(&'static str),
    InvalidParameter(String),
    UnsupportedParameter(String),
}

/// The decoder of a DATA body: removes dot-stuffing and finds the `<CRLF>.<CRLF>` end. A bare LF or CR
/// never ends the body.
pub trait BodyDecoder: Send {
    /// Decodes from `input` into `out`; returns the bytes consumed and whether the end was found (the
    /// bytes after the end are not consumed).
    fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> (usize, bool);
}

/// The wire format of SMTP commands.
pub trait SmtpCodec: Send + Sync {
    /// Parses one command line (ending with LF).
    fn command(&self, line: &[u8]) -> Result<Command, CommandError>;
    /// A decoder for a DATA body that starts right after the DATA command line.
    fn body_decoder(&self) -> Box<dyn BodyDecoder>;
}

/// [`SmtpCodec`] on top of `smtp-proto`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProtoCodec;

fn printable(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect::<String>()
}

impl SmtpCodec for ProtoCodec {
    fn command(&self, line: &[u8]) -> Result<Command, CommandError> {
        use smtp_proto::{Error, Request};
        let mut it = line.iter();
        let req = match Request::parse(&mut it) {
            Ok(r) => r,
            Err(e) => {
                return Err(match e {
                    Error::UnknownCommand | Error::NeedsMoreData { .. } => CommandError::Unknown,
                    Error::InvalidSenderAddress => CommandError::BadSender,
                    Error::InvalidRecipientAddress => CommandError::BadRecipient,
                    Error::SyntaxError { syntax } => CommandError::Syntax(syntax),
                    Error::InvalidParameter { param } => {
                        CommandError::InvalidParameter(param.to_string())
                    }
                    Error::UnsupportedParameter { param } => {
                        CommandError::UnsupportedParameter(printable(&param))
                    }
                    Error::ResponseTooLong | Error::InvalidResponse { .. } => {
                        CommandError::Syntax("line too long")
                    }
                });
            }
        };
        Ok(match req {
            Request::Ehlo { host } => Command::Ehlo {
                host: host.into_owned(),
            },
            Request::Helo { host } => Command::Helo {
                host: host.into_owned(),
            },
            Request::Lhlo { .. } => Command::NotImplemented("LHLO"),
            Request::Mail { from } => {
                let body = if from.flags & smtp_proto::MAIL_BODY_BINARYMIME != 0 {
                    BodyType::BinaryMime
                } else if from.flags & smtp_proto::MAIL_BODY_8BITMIME != 0 {
                    BodyType::EightBitMime
                } else {
                    BodyType::SevenBit
                };
                Command::Mail {
                    address: from.address.into_owned(),
                    size: (from.size > 0).then_some(from.size),
                    smtputf8: from.flags & smtp_proto::MAIL_SMTPUTF8 != 0,
                    body,
                }
            }
            Request::Rcpt { to } => {
                let a = to.address.into_owned();
                if a.is_empty() {
                    Command::Rcpt {
                        address: "postmaster".into(),
                    }
                } else {
                    Command::Rcpt { address: a }
                }
            }
            Request::Bdat {
                chunk_size,
                is_last,
            } => Command::Bdat {
                size: chunk_size,
                last: is_last,
            },
            Request::Auth {
                mechanism,
                initial_response,
            } => {
                let m = match mechanism {
                    smtp_proto::AUTH_PLAIN => Mechanism::Plain,
                    smtp_proto::AUTH_LOGIN => Mechanism::Login,
                    smtp_proto::AUTH_CRAM_MD5 => Mechanism::CramMd5,
                    _ => Mechanism::Other,
                };
                let ir = initial_response.trim();
                Command::Auth {
                    mechanism: m,
                    initial: (!ir.is_empty()).then(|| ir.to_string()),
                }
            }
            Request::Noop { .. } => Command::Noop,
            Request::Vrfy { .. } => Command::Vrfy,
            Request::Expn { .. } => Command::NotImplemented("EXPN"),
            Request::Help { .. } => Command::NotImplemented("HELP"),
            Request::Etrn { .. } => Command::NotImplemented("ETRN"),
            Request::Atrn { .. } => Command::NotImplemented("ATRN"),
            Request::Burl { .. } => Command::NotImplemented("BURL"),
            Request::StartTls => Command::StartTls,
            Request::Data => Command::Data,
            Request::Rset => Command::Rset,
            Request::Quit => Command::Quit,
        })
    }

    fn body_decoder(&self) -> Box<dyn BodyDecoder> {
        let mut inner = smtp_proto::request::receiver::DataReceiver::new();
        let mut scratch = Vec::new();
        // The body starts after the CRLF of the DATA line: prime the decoder with it so that a first
        // line `.` ends an empty body and a leading dot is unstuffed.
        inner.ingest(&mut b"\r\n".iter(), &mut scratch);
        Box::new(ProtoBody { inner })
    }
}

struct ProtoBody {
    inner: smtp_proto::request::receiver::DataReceiver,
}

impl BodyDecoder for ProtoBody {
    fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> (usize, bool) {
        let mut it = input.iter();
        let done = self.inner.ingest(&mut it, out);
        (input.len() - it.as_slice().len(), done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Result<Command, CommandError> {
        ProtoCodec.command(s.as_bytes())
    }

    fn body(chunks: &[&[u8]]) -> (Vec<u8>, bool, Vec<u8>) {
        let mut d = ProtoCodec.body_decoder();
        let mut out = Vec::new();
        let mut rest = Vec::new();
        let mut done = false;
        for c in chunks {
            if done {
                rest.extend_from_slice(c);
                continue;
            }
            let (n, d2) = d.feed(c, &mut out);
            done = d2;
            if done {
                rest.extend_from_slice(&c[n..]);
            } else {
                assert_eq!(n, c.len());
            }
        }
        (out, done, rest)
    }

    #[test]
    fn commands() {
        assert_eq!(
            cmd("EHLO client.example.test\r\n"),
            Ok(Command::Ehlo {
                host: "client.example.test".into()
            })
        );
        assert_eq!(
            cmd("mail from:<a@example.com> SIZE=100 BODY=8BITMIME SMTPUTF8\r\n"),
            Ok(Command::Mail {
                address: "a@example.com".into(),
                size: Some(100),
                smtputf8: true,
                body: BodyType::EightBitMime
            })
        );
        assert_eq!(
            cmd("MAIL FROM:<>\r\n"),
            Ok(Command::Mail {
                address: String::new(),
                size: None,
                smtputf8: false,
                body: BodyType::SevenBit
            })
        );
        assert_eq!(
            cmd("RCPT TO:<Postmaster>\r\n"),
            Ok(Command::Rcpt {
                address: "postmaster".into()
            })
        );
        assert_eq!(
            cmd("RCPT TO:<jürgen@bücher.example>\r\n"),
            Ok(Command::Rcpt {
                address: "jürgen@bücher.example".into()
            })
        );
        assert_eq!(
            cmd("BDAT 10 LAST\r\n"),
            Ok(Command::Bdat {
                size: 10,
                last: true
            })
        );
        assert_eq!(
            cmd("AUTH PLAIN AGEAYg==\r\n"),
            Ok(Command::Auth {
                mechanism: Mechanism::Plain,
                initial: Some("AGEAYg==".into())
            })
        );
        assert_eq!(
            cmd("AUTH CRAM-MD5\r\n"),
            Ok(Command::Auth {
                mechanism: Mechanism::CramMd5,
                initial: None
            })
        );
        assert_eq!(
            cmd("AUTH XOAUTH2 abc\r\n"),
            Ok(Command::Auth {
                mechanism: Mechanism::Other,
                initial: Some("abc".into())
            })
        );
        assert_eq!(cmd("STARTTLS\r\n"), Ok(Command::StartTls));
        assert_eq!(cmd("EXPN list\r\n"), Ok(Command::NotImplemented("EXPN")));
        assert_eq!(cmd("vrfy bob\r\n"), Ok(Command::Vrfy));
        assert_eq!(cmd("quit\n"), Ok(Command::Quit));
        assert_eq!(cmd("FOO bar\r\n"), Err(CommandError::Unknown));
        assert_eq!(cmd("MAIL FROM:<a b>\r\n"), Err(CommandError::BadSender));
        assert!(cmd("RCPT TO:<@>\r\n").is_err());
        assert!(matches!(
            cmd("MAIL FROM:a@b\r\n"),
            Err(CommandError::Syntax(_))
        ));
        assert!(matches!(cmd("EHLO\r\n"), Err(CommandError::Syntax(_))));
    }

    #[test]
    fn data_ends_only_on_crlf_dot_crlf() {
        let (out, done, rest) = body(&[b"Subject: a\r\n\r\nline\r\n..dot\r\n.\r\nQUIT\r\n"]);
        assert!(done);
        assert_eq!(out, b"Subject: a\r\n\r\nline\r\n.dot\r\n");
        assert_eq!(rest, b"QUIT\r\n");
        let (out, done, _) = body(&[b"a\n.\nb\r.\rc\r\n"]);
        assert!(!done);
        assert_eq!(out, b"a\n.\nb\r.\rc\r\n");
        let (out, done, _) = body(&[b"a\r\n.\nb\n.\r\nc\r\n.\r\n"]);
        assert!(done);
        assert_eq!(out, b"a\r\n\nb\n.\r\nc\r\n");
        let (out, done, _) = body(&[b"a\r", b"\n", b".", b"\r", b"\n"]);
        assert!(done);
        assert_eq!(out, b"a\r\n");
    }

    #[test]
    fn empty_body_and_leading_dot() {
        let (out, done, rest) = body(&[b".\r\nNOOP\r\n"]);
        assert!(done);
        assert!(out.is_empty());
        assert_eq!(rest, b"NOOP\r\n");
        let (out, done, _) = body(&[b"..x\r\n.\r\n"]);
        assert!(done);
        assert_eq!(out, b".x\r\n");
    }
}
