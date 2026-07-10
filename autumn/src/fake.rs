//! Deterministic fake-data generation.
//!
//! This module backs the factory `.fake()` feature (issue #1343): it produces
//! realistic-looking values — names, emails, sentences, timestamps, and so on —
//! drawn from a per-thread pseudo-random generator.
//!
//! # Determinism
//!
//! The generator can run in two modes:
//!
//! - **Deterministic**: when the `AUTUMN_FAKE_SEED` environment variable is set
//!   to a `u64`, or when [`reseed`] is called explicitly, the thread-local RNG
//!   is seeded from that value. The exact same sequence of calls then produces
//!   the exact same values on every run — ideal for golden tests and
//!   reproducible fixtures. In this mode time-based helpers such as
//!   [`recent_datetime`] anchor to a fixed base instant rather than the wall
//!   clock, so even timestamps are reproducible.
//! - **Random**: with no seed configured, the RNG is seeded from OS entropy and
//!   output varies per run.
//!
//! The RNG is [`rand_chacha::ChaCha8Rng`], chosen because ChaCha is portable and
//! reproducible across platforms given the same seed.

use std::cell::RefCell;

use chrono::{DateTime, Utc};
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rust_decimal::Decimal;
use uuid::Uuid;

/// Per-thread generator state.
struct FakeState {
    rng: ChaCha8Rng,
    /// True when the RNG was seeded deterministically (via `AUTUMN_FAKE_SEED` or
    /// [`reseed`]). Drives whether time helpers anchor to a fixed instant.
    deterministic: bool,
}

thread_local! {
    static RNG: RefCell<Option<FakeState>> = const { RefCell::new(None) };
}

/// The fixed base instant used by time helpers in deterministic mode.
/// Chosen as `2024-01-01T00:00:00Z`.
const DETERMINISTIC_BASE_EPOCH_SECS: i64 = 1_704_067_200;

/// Ensure the thread-local generator is initialized, seeding it on first use.
///
/// Seeds deterministically from `AUTUMN_FAKE_SEED` when that env var parses as a
/// `u64`; otherwise seeds from OS entropy.
fn ensure_init(cell: &mut Option<FakeState>) {
    if cell.is_none() {
        let seed = std::env::var("AUTUMN_FAKE_SEED")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        *cell = Some(seed.map_or_else(
            || FakeState {
                rng: ChaCha8Rng::from_os_rng(),
                deterministic: false,
            },
            |seed| FakeState {
                rng: ChaCha8Rng::seed_from_u64(seed),
                deterministic: true,
            },
        ));
    }
}

/// Run `f` with a mutable reference to the thread-local RNG.
fn with_rng<R>(f: impl FnOnce(&mut ChaCha8Rng) -> R) -> R {
    RNG.with(|cell| {
        let mut borrow = cell.borrow_mut();
        ensure_init(&mut borrow);
        f(&mut borrow.as_mut().expect("rng initialized").rng)
    })
}

/// Whether the generator is currently in deterministic mode.
fn is_deterministic() -> bool {
    RNG.with(|cell| {
        let mut borrow = cell.borrow_mut();
        ensure_init(&mut borrow);
        borrow.as_ref().expect("rng initialized").deterministic
    })
}

/// Force the thread-local generator into deterministic mode seeded from `seed`.
///
/// After this call, a fixed sequence of generator calls yields a fixed sequence
/// of values. Primarily intended for tests and reproducible fixtures.
pub fn reseed(seed: u64) {
    RNG.with(|cell| {
        *cell.borrow_mut() = Some(FakeState {
            rng: ChaCha8Rng::seed_from_u64(seed),
            deterministic: true,
        });
    });
}

/// Pick a random element from a non-empty static list.
fn pick(list: &[&'static str]) -> &'static str {
    debug_assert!(!list.is_empty(), "fake: word list must be non-empty");
    let idx = with_rng(|r| r.random_range(0..list.len()));
    list[idx]
}

// ── People ──────────────────────────────────────────────────────────────────

/// A random first name, e.g. `"Olivia"`.
#[must_use]
pub fn first_name() -> String {
    pick(FIRST_NAMES).to_string()
}

/// A random last name, e.g. `"Nguyen"`.
#[must_use]
pub fn last_name() -> String {
    pick(LAST_NAMES).to_string()
}

/// A random full name (`"First Last"`).
#[must_use]
pub fn name() -> String {
    format!("{} {}", first_name(), last_name())
}

/// A lowercase, space-free username derived from a name plus digits,
/// e.g. `"olivianguyen473"`.
#[must_use]
pub fn username() -> String {
    let first = pick(FIRST_NAMES).to_ascii_lowercase();
    let last = pick(LAST_NAMES).to_ascii_lowercase();
    let n: u32 = with_rng(|r| r.random_range(0..1000));
    format!("{first}{last}{n}")
}

/// A random email address containing exactly one `@`, e.g.
/// `"olivia473@example.com"`.
#[must_use]
pub fn email() -> String {
    let first = pick(FIRST_NAMES).to_ascii_lowercase();
    let n: u32 = with_rng(|r| r.random_range(0..1000));
    let domain = pick(DOMAINS);
    format!("{first}{n}@{domain}")
}

// ── Text ────────────────────────────────────────────────────────────────────

/// A single lorem-style word.
#[must_use]
pub fn word() -> String {
    pick(LOREM).to_string()
}

/// `n` space-joined lorem words. Returns an empty string when `n == 0`.
#[must_use]
pub fn words(n: usize) -> String {
    let mut out = String::new();
    for i in 0..n {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(pick(LOREM));
    }
    out
}

/// A capitalized sentence of several words ending in `'.'`.
#[must_use]
pub fn sentence() -> String {
    let n = with_rng(|r| r.random_range(4..12));
    let mut s = words(n);
    if let Some(head) = s.get_mut(0..1) {
        head.make_ascii_uppercase();
    }
    s.push('.');
    s
}

/// A paragraph of several sentences joined by spaces.
#[must_use]
pub fn paragraph() -> String {
    let n = with_rng(|r| r.random_range(3..7));
    let mut out = String::new();
    for i in 0..n {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&sentence());
    }
    out
}

// ── Scalars ─────────────────────────────────────────────────────────────────

/// A random URL beginning with `"https://"`.
#[must_use]
pub fn url() -> String {
    format!("https://{}/{}", pick(DOMAINS), pick(LOREM))
}

/// A random boolean.
#[must_use]
pub fn boolean() -> bool {
    with_rng(|r| r.random_bool(0.5))
}

/// A random integer within the inclusive range `[lo, hi]`.
///
/// If `lo >= hi`, returns `lo` (so the result is always within `[lo, hi]`).
#[must_use]
pub fn int_range(lo: i64, hi: i64) -> i64 {
    if lo >= hi {
        return lo;
    }
    with_rng(|r| r.random_range(lo..=hi))
}

/// A random non-negative [`Decimal`] with two fractional digits (0.00–9999.99).
#[must_use]
pub fn decimal() -> Decimal {
    let cents = with_rng(|r| r.random_range(0..1_000_000_i64));
    Decimal::new(cents, 2)
}

/// A random `f64` in `[0, 10000)`, for `f32`/`f64` fields.
#[must_use]
pub fn decimal_f64() -> f64 {
    with_rng(|r| r.random_range(0.0..10_000.0))
}

/// A timestamp within roughly the last 30 days.
///
/// In deterministic mode the offset is subtracted from a fixed base instant
/// (`2024-01-01T00:00:00Z`) so golden data is reproducible; otherwise it is
/// subtracted from [`Utc::now`].
#[must_use]
pub fn recent_datetime() -> DateTime<Utc> {
    const THIRTY_DAYS_SECS: i64 = 30 * 24 * 60 * 60;
    let offset = with_rng(|r| r.random_range(0..THIRTY_DAYS_SECS));
    // In deterministic mode, anchor to a fixed instant so timestamps reproduce.
    // `UNIX_EPOCH + N seconds` is infallible (no panic path).
    let base = if is_deterministic() {
        DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(DETERMINISTIC_BASE_EPOCH_SECS)
    } else {
        Utc::now()
    };
    base - chrono::Duration::seconds(offset)
}

/// A random v4 UUID drawn from the seeded RNG (so it is reproducible under a
/// fixed seed, unlike [`Uuid::new_v4`] which uses OS entropy directly).
#[must_use]
pub fn uuid() -> Uuid {
    let mut bytes = [0u8; 16];
    with_rng(|r| r.fill_bytes(&mut bytes));
    // Set the version (4) and variant (RFC 4122) bits.
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    Uuid::from_bytes(bytes)
}

// ── Word lists ──────────────────────────────────────────────────────────────
// Bundled static lists, sized so name()/email()/sentence() have high
// cardinality (AC3). ~120 first names, ~120 last names, ~150 lorem words.

/// First names (~120).
static FIRST_NAMES: &[&str] = &[
    "Olivia",
    "Liam",
    "Emma",
    "Noah",
    "Ava",
    "Oliver",
    "Sophia",
    "Elijah",
    "Isabella",
    "James",
    "Mia",
    "William",
    "Amelia",
    "Benjamin",
    "Harper",
    "Lucas",
    "Evelyn",
    "Henry",
    "Abigail",
    "Alexander",
    "Emily",
    "Mason",
    "Elizabeth",
    "Michael",
    "Sofia",
    "Ethan",
    "Avery",
    "Daniel",
    "Ella",
    "Jacob",
    "Scarlett",
    "Logan",
    "Grace",
    "Jackson",
    "Chloe",
    "Levi",
    "Victoria",
    "Sebastian",
    "Riley",
    "Mateo",
    "Aria",
    "Jack",
    "Lily",
    "Owen",
    "Aubrey",
    "Theodore",
    "Zoey",
    "Aiden",
    "Penelope",
    "Samuel",
    "Lillian",
    "Joseph",
    "Addison",
    "John",
    "Layla",
    "David",
    "Natalie",
    "Wyatt",
    "Camila",
    "Matthew",
    "Hannah",
    "Luke",
    "Brooklyn",
    "Asher",
    "Zoe",
    "Carter",
    "Nora",
    "Julian",
    "Leah",
    "Grayson",
    "Savannah",
    "Leo",
    "Audrey",
    "Jayden",
    "Claire",
    "Gabriel",
    "Eleanor",
    "Isaac",
    "Skylar",
    "Lincoln",
    "Ellie",
    "Anthony",
    "Samantha",
    "Hudson",
    "Stella",
    "Dylan",
    "Paisley",
    "Ezra",
    "Violet",
    "Thomas",
    "Mila",
    "Charles",
    "Allison",
    "Christopher",
    "Alexa",
    "Jaxon",
    "Anna",
    "Maverick",
    "Hazel",
    "Josiah",
    "Aaliyah",
    "Isaiah",
    "Ariana",
    "Andrew",
    "Gabriella",
    "Elias",
    "Alice",
    "Joshua",
    "Sarah",
    "Nathan",
    "Ruby",
    "Caleb",
    "Eva",
    "Ryan",
    "Serenity",
    "Adrian",
    "Autumn",
    "Miles",
    "Quinn",
    "Eli",
    "Nova",
];

/// Last names (~120).
static LAST_NAMES: &[&str] = &[
    "Smith",
    "Johnson",
    "Williams",
    "Brown",
    "Jones",
    "Garcia",
    "Miller",
    "Davis",
    "Rodriguez",
    "Martinez",
    "Hernandez",
    "Lopez",
    "Gonzalez",
    "Wilson",
    "Anderson",
    "Thomas",
    "Taylor",
    "Moore",
    "Jackson",
    "Martin",
    "Lee",
    "Perez",
    "Thompson",
    "White",
    "Harris",
    "Sanchez",
    "Clark",
    "Ramirez",
    "Lewis",
    "Robinson",
    "Walker",
    "Young",
    "Allen",
    "King",
    "Wright",
    "Scott",
    "Torres",
    "Nguyen",
    "Hill",
    "Flores",
    "Green",
    "Adams",
    "Nelson",
    "Baker",
    "Hall",
    "Rivera",
    "Campbell",
    "Mitchell",
    "Carter",
    "Roberts",
    "Gomez",
    "Phillips",
    "Evans",
    "Turner",
    "Diaz",
    "Parker",
    "Cruz",
    "Edwards",
    "Collins",
    "Reyes",
    "Stewart",
    "Morris",
    "Morales",
    "Murphy",
    "Cook",
    "Rogers",
    "Gutierrez",
    "Ortiz",
    "Morgan",
    "Cooper",
    "Peterson",
    "Bailey",
    "Reed",
    "Kelly",
    "Howard",
    "Ramos",
    "Kim",
    "Cox",
    "Ward",
    "Richardson",
    "Watson",
    "Brooks",
    "Chavez",
    "Wood",
    "James",
    "Bennett",
    "Gray",
    "Mendoza",
    "Ruiz",
    "Hughes",
    "Price",
    "Alvarez",
    "Castillo",
    "Sanders",
    "Patel",
    "Myers",
    "Long",
    "Ross",
    "Foster",
    "Jimenez",
    "Powell",
    "Jenkins",
    "Perry",
    "Russell",
    "Sullivan",
    "Bell",
    "Coleman",
    "Butler",
    "Henderson",
    "Barnes",
    "Gonzales",
    "Fisher",
    "Vasquez",
    "Simmons",
    "Romero",
    "Jordan",
    "Patterson",
    "Alexander",
    "Hamilton",
    "Graham",
    "Reynolds",
];

/// Lorem-ipsum vocabulary (~150).
static LOREM: &[&str] = &[
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
    "eiusmod",
    "tempor",
    "incididunt",
    "ut",
    "labore",
    "et",
    "dolore",
    "magna",
    "aliqua",
    "enim",
    "ad",
    "minim",
    "veniam",
    "quis",
    "nostrud",
    "exercitation",
    "ullamco",
    "laboris",
    "nisi",
    "aliquip",
    "ex",
    "ea",
    "commodo",
    "consequat",
    "duis",
    "aute",
    "irure",
    "in",
    "reprehenderit",
    "voluptate",
    "velit",
    "esse",
    "cillum",
    "eu",
    "fugiat",
    "nulla",
    "pariatur",
    "excepteur",
    "sint",
    "occaecat",
    "cupidatat",
    "non",
    "proident",
    "sunt",
    "culpa",
    "qui",
    "officia",
    "deserunt",
    "mollit",
    "anim",
    "id",
    "est",
    "laborum",
    "perspiciatis",
    "unde",
    "omnis",
    "iste",
    "natus",
    "error",
    "voluptatem",
    "accusantium",
    "doloremque",
    "laudantium",
    "totam",
    "rem",
    "aperiam",
    "eaque",
    "ipsa",
    "quae",
    "ab",
    "illo",
    "inventore",
    "veritatis",
    "quasi",
    "architecto",
    "beatae",
    "vitae",
    "dicta",
    "explicabo",
    "nemo",
    "ipsam",
    "quia",
    "voluptas",
    "aspernatur",
    "aut",
    "odit",
    "fugit",
    "consequuntur",
    "magni",
    "dolores",
    "eos",
    "ratione",
    "sequi",
    "nesciunt",
    "neque",
    "porro",
    "quisquam",
    "dolorem",
    "adipisci",
    "numquam",
    "eius",
    "modi",
    "tempora",
    "incidunt",
    "magnam",
    "quaerat",
    "voluptatem",
    "minus",
    "quod",
    "maxime",
    "placeat",
    "facere",
    "possimus",
    "assumenda",
    "repellendus",
    "temporibus",
    "quibusdam",
    "officiis",
    "debitis",
    "rerum",
    "necessitatibus",
    "saepe",
    "eveniet",
    "voluptates",
    "repudiandae",
    "recusandae",
    "itaque",
    "earum",
    "hic",
    "tenetur",
    "sapiente",
    "delectus",
    "reiciendis",
    "voluptatibus",
    "maiores",
    "alias",
    "perferendis",
    "doloribus",
    "asperiores",
    "repellat",
];

/// Email/URL domains (mixes several second-level names and TLDs).
static DOMAINS: &[&str] = &[
    "example.com",
    "example.org",
    "example.net",
    "test.com",
    "mail.com",
    "acme.io",
    "globex.dev",
    "initech.co",
    "umbrella.app",
    "hooli.tech",
    "stark.io",
    "wayne.net",
];
