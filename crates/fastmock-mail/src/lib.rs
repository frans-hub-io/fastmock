//! FastMock mail: the message model and its parsing, routing to spaces, labels, matching and searches,
//! MIME building for injected messages, mail settings and the capture pipeline ([`MailService`]) that
//! the protocol servers and the admin API share. Nothing here depends on the HTTP stack; the server
//! crate wires it in. See README.md for the module layout and the seams of the later phases.

pub mod address;
pub mod chaos;
pub mod compose;
pub mod config;
pub mod extract;
pub mod imap;
pub mod labels;
pub mod matching;
pub mod parse;
pub mod pop3;
pub mod query;
pub mod routing;
pub mod service;
pub mod smtp;
pub mod tls;

pub use config::MailConfig;
pub use fastmock_core::mail::*;
pub use matching::{CompiledMailMatch, MailMatch, MatchView};
pub use parse::{MailDetail, ParseLimits, ParsedMail};
pub use routing::{MailDomains, Route};
pub use service::{AcceptError, Destination, Incoming, MailService};
