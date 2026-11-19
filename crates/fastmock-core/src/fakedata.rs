use chrono::{DateTime, TimeDelta, Utc};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use sha2::{Digest, Sha256};

const FIRST_NAMES: &[&str] = &[
    "Olivia",
    "Liam",
    "Emma",
    "Noah",
    "Amelia",
    "Oliver",
    "Sophia",
    "Elijah",
    "Mia",
    "Lucas",
    "Charlotte",
    "Mateo",
    "Ava",
    "Levi",
    "Isla",
    "Leo",
    "Maya",
    "Ezra",
    "Chloe",
    "Hugo",
    "Lea",
    "Ivan",
    "Sofia",
    "Taras",
    "Anna",
    "Marco",
    "Yuki",
    "Aarav",
    "Fatima",
    "Omar",
];
const LAST_NAMES: &[&str] = &[
    "Smith", "Johnson", "Williams", "Brown", "Jones", "Garcia", "Miller", "Davis", "Martinez",
    "Lopez", "Wilson", "Anderson", "Taylor", "Thomas", "Moore", "Martin", "Lee", "Clark", "Lewis",
    "Walker", "Hall", "Young", "King", "Wright", "Scott", "Green", "Baker", "Adams", "Nelson",
    "Carter",
];
const CITIES: &[&str] = &[
    "Springfield",
    "Riverside",
    "Fairview",
    "Franklin",
    "Greenville",
    "Bristol",
    "Clinton",
    "Georgetown",
    "Salem",
    "Madison",
    "Kingston",
    "Arlington",
    "Ashland",
    "Burlington",
    "Manchester",
    "Oxford",
    "Dover",
    "Milton",
    "Newport",
    "Hudson",
];
const COUNTRIES: &[(&str, &str)] = &[
    ("Ukraine", "UA"),
    ("Poland", "PL"),
    ("Germany", "DE"),
    ("France", "FR"),
    ("Spain", "ES"),
    ("Italy", "IT"),
    ("Netherlands", "NL"),
    ("Sweden", "SE"),
    ("Norway", "NO"),
    ("Finland", "FI"),
    ("United Kingdom", "GB"),
    ("Ireland", "IE"),
    ("United States", "US"),
    ("Canada", "CA"),
    ("Mexico", "MX"),
    ("Brazil", "BR"),
    ("Argentina", "AR"),
    ("Japan", "JP"),
    ("South Korea", "KR"),
    ("India", "IN"),
    ("Australia", "AU"),
    ("New Zealand", "NZ"),
    ("South Africa", "ZA"),
    ("Egypt", "EG"),
];
const STREETS: &[&str] = &[
    "Main",
    "Oak",
    "Pine",
    "Maple",
    "Cedar",
    "Elm",
    "Washington",
    "Lake",
    "Hill",
    "Park",
    "River",
    "Sunset",
    "Church",
    "Mill",
    "Spring",
    "Forest",
];
const STREET_SUFFIXES: &[&str] = &[
    "Street",
    "Avenue",
    "Road",
    "Lane",
    "Boulevard",
    "Drive",
    "Way",
];
const COMPANY_WORDS: &[&str] = &[
    "Acme",
    "Globex",
    "Initech",
    "Umbrella",
    "Stark",
    "Wayne",
    "Hooli",
    "Vandelay",
    "Wonka",
    "Cyberdyne",
    "Soylent",
    "Tyrell",
    "Aperture",
    "Massive",
    "Northwind",
    "Contoso",
];
const COMPANY_SUFFIXES: &[&str] = &["LLC", "Inc", "Group", "Ltd", "Labs", "Systems", "Partners"];
const DOMAINS: &[&str] = &["example.com", "example.org", "example.net"];
const WORDS: &[&str] = &[
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra", "tango",
];
const JOB_TITLES: &[&str] = &[
    "Engineer",
    "Designer",
    "Analyst",
    "Manager",
    "Consultant",
    "Architect",
    "Developer",
    "Accountant",
    "Coordinator",
    "Specialist",
    "Administrator",
    "Technician",
];
const COLORS: &[&str] = &[
    "red", "green", "blue", "yellow", "purple", "orange", "black", "white", "gray", "teal",
];
const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

pub const KINDS: &[&str] = &[
    "randomFirstName",
    "randomLastName",
    "randomFullName",
    "randomEmail",
    "randomUserName",
    "randomPhoneNumber",
    "randomCity",
    "randomCountry",
    "randomCountryCode",
    "randomStreetAddress",
    "randomCompanyName",
    "randomJobTitle",
    "randomWord",
    "randomColor",
    "randomBoolean",
    "randomAlphaNumeric",
    "randomIP",
    "randomIPv6",
    "randomUUID",
    "randomUrl",
    "randomDateTime",
];

pub fn rng(seed: Option<&str>, kind: &str) -> StdRng {
    match seed {
        Some(s) => {
            let d = Sha256::digest(format!("{kind}\u{0}{s}").as_bytes());
            let mut b = [0u8; 32];
            b.copy_from_slice(&d);
            StdRng::from_seed(b)
        }
        None => StdRng::from_rng(&mut rand::rng()),
    }
}

fn pick<'a>(r: &mut StdRng, list: &[&'a str]) -> &'a str {
    list[r.random_range(0..list.len())]
}

fn alnum(r: &mut StdRng, n: usize) -> String {
    (0..n)
        .map(|_| char::from(ALNUM[r.random_range(0..ALNUM.len())]))
        .collect()
}

pub struct Options {
    pub length: usize,
    pub now: DateTime<Utc>,
    pub days: i64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            length: 10,
            now: Utc::now(),
            days: 365,
        }
    }
}

pub fn generate(kind: &str, r: &mut StdRng, o: &Options) -> Option<serde_json::Value> {
    use serde_json::Value;
    let s = |v: String| Some(Value::String(v));
    match kind {
        "randomFirstName" => s(pick(r, FIRST_NAMES).into()),
        "randomLastName" => s(pick(r, LAST_NAMES).into()),
        "randomFullName" => {
            let f = pick(r, FIRST_NAMES);
            s(format!("{f} {}", pick(r, LAST_NAMES)))
        }
        "randomUserName" => {
            let f = pick(r, FIRST_NAMES).to_lowercase();
            s(format!("{f}{}", r.random_range(1..1000)))
        }
        "randomEmail" => {
            let f = pick(r, FIRST_NAMES).to_lowercase();
            let l = pick(r, LAST_NAMES).to_lowercase();
            s(format!("{f}.{l}@{}", pick(r, DOMAINS)))
        }
        "randomPhoneNumber" => s(format!(
            "+1-{:03}-{:03}-{:04}",
            r.random_range(200..1000),
            r.random_range(200..1000),
            r.random_range(0..10000)
        )),
        "randomCity" => s(pick(r, CITIES).into()),
        "randomCountry" => s(COUNTRIES[r.random_range(0..COUNTRIES.len())].0.into()),
        "randomCountryCode" => s(COUNTRIES[r.random_range(0..COUNTRIES.len())].1.into()),
        "randomStreetAddress" => {
            let n = r.random_range(1..10000);
            let st = pick(r, STREETS);
            s(format!("{n} {st} {}", pick(r, STREET_SUFFIXES)))
        }
        "randomCompanyName" => {
            let w = pick(r, COMPANY_WORDS);
            s(format!("{w} {}", pick(r, COMPANY_SUFFIXES)))
        }
        "randomJobTitle" => s(pick(r, JOB_TITLES).into()),
        "randomWord" => s(pick(r, WORDS).into()),
        "randomColor" => s(pick(r, COLORS).into()),
        "randomBoolean" => Some(Value::Bool(r.random())),
        "randomAlphaNumeric" => s(alnum(r, o.length)),
        "randomIP" => s(format!(
            "{}.{}.{}.{}",
            r.random_range(1..224u8),
            r.random::<u8>(),
            r.random::<u8>(),
            r.random_range(1..255u8)
        )),
        "randomIPv6" => s((0..8)
            .map(|_| format!("{:x}", r.random::<u16>()))
            .collect::<Vec<_>>()
            .join(":")),
        "randomUUID" => {
            let b: [u8; 16] = r.random();
            s(uuid::Builder::from_random_bytes(b).into_uuid().to_string())
        }
        "randomUrl" => s(format!("https://{}/{}", pick(r, DOMAINS), pick(r, WORDS))),
        "randomDateTime" => {
            let span = o.days.unsigned_abs().clamp(1, 36_500) as i64 * 86_400;
            let secs = r.random_range(-span..=span);
            let t = TimeDelta::try_seconds(secs)
                .and_then(|d| o.now.checked_add_signed(d))
                .unwrap_or(o.now);
            s(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_generates_and_seeds_are_deterministic() {
        let o = Options::default();
        for k in KINDS {
            let a = generate(k, &mut rng(Some("42"), k), &o).unwrap();
            let b = generate(k, &mut rng(Some("42"), k), &o).unwrap();
            assert_eq!(a, b, "{k}");
            assert!(!a.is_null(), "{k}");
        }
        assert!(generate("nope", &mut rng(None, "x"), &o).is_none());
        let email = generate("randomEmail", &mut rng(None, "e"), &o).unwrap();
        assert!(email.as_str().unwrap().contains('@'));
        let ip = generate("randomIP", &mut rng(None, "i"), &o).unwrap();
        assert!(ip.as_str().unwrap().parse::<std::net::Ipv4Addr>().is_ok());
    }
}
