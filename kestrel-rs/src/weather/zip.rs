//! ZIP code to coordinate, out of the table shipped with the binary.
//!
//! The National Weather Service API is addressed by latitude and longitude and
//! has no idea what a ZIP code is, so something has to bridge the two. Every
//! free service that will do it is somebody's side project, and setup that
//! depends on one breaks the day it goes away — so the table is carried
//! instead, the same call the Roku channel already made.
//!
//! `assets/data/zipcodes.txt` comes from the Census Bureau's ZCTA gazetteer,
//! sorted by code, one fixed-width record per line:
//!
//! ```text
//!     01001 +42.062 -072.626
//!     |     |       |
//!     0     6       14        23 bytes
//! ```
//!
//! Fixed width is what makes it searchable without parsing 33,791 lines into a
//! map: the nth record starts at `n * 23`, so this is a binary search over the
//! file's own bytes and touches about fifteen of them. That is cheap enough
//! that there is no table to build at startup and nothing to keep in memory
//! beyond the bytes already in the binary.
//!
//! This runs once, when a ZIP is typed into preferences. What gets stored is
//! the coordinate, so nothing reads it again while the app is running.

/// The gazetteer, embedded so a single binary needs no data file beside it —
/// the same call the icon and font assets make.
const TABLE: &[u8] = include_bytes!("../../assets/data/zipcodes.txt");

const RECORD: usize = 23;

/// A coordinate as the strings that go in the URL, so nothing downstream has to
/// think about float formatting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub zip: String,
    pub lat: String,
    pub lon: String,
}

pub fn lookup(zip: &str) -> Result<Location, String> {
    let code = normalise(zip).ok_or("a ZIP code is five digits")?;
    find(TABLE, &code).ok_or_else(|| {
        // Not necessarily a mistyped code: the gazetteer covers ZIP code
        // tabulation areas, which leave out the few thousand codes that are one
        // building or a row of PO boxes. Saying so is more use than "no such
        // ZIP code", which sends somebody looking for a typo that is not there.
        format!("no forecast area for {code} — try a neighbouring ZIP code")
    })
}

fn find(table: &[u8], code: &str) -> Option<Location> {
    let records = table.len() / RECORD;
    let (mut low, mut high) = (0usize, records);

    while low < high {
        let middle = (low + high) / 2;
        let at = middle * RECORD;
        let found = std::str::from_utf8(table.get(at..at + 5)?).ok()?;

        match found.cmp(code) {
            std::cmp::Ordering::Equal => {
                return Some(Location {
                    zip: code.to_string(),
                    lat: trim_number(std::str::from_utf8(table.get(at + 6..at + 13)?).ok()?),
                    lon: trim_number(std::str::from_utf8(table.get(at + 14..at + 22)?).ok()?),
                })
            }
            std::cmp::Ordering::Less => low = middle + 1,
            std::cmp::Ordering::Greater => high = middle,
        }
    }
    None
}

/// Five digits, or nothing. Accepts ZIP+4 and anything spaced or hyphenated
/// around it, because that is how a code gets pasted out of an address.
fn normalise(zip: &str) -> Option<String> {
    let mut digits = String::new();
    for ch in zip.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            if digits.len() == 5 {
                return Some(digits);
            }
        } else if ch != ' ' && ch != '-' {
            return None;
        }
    }
    None
}

/// "+42.062" and "-072.626" as they are written in the table, to "42.062" and
/// "-72.626" as they are written in a URL. The padding is there to keep every
/// record the same width, and is no use past that.
fn trim_number(field: &str) -> String {
    let text = field.trim();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text.strip_prefix('+').unwrap_or(text)),
    };

    // Leading zeros are padding, but "0.5" must keep the one before the point.
    let trimmed = digits.trim_start_matches('0');
    let trimmed = if trimmed.is_empty() || trimmed.starts_with('.') {
        format!("0{trimmed}")
    } else {
        trimmed.to_string()
    };
    format!("{sign}{trimmed}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_table_is_the_shape_the_search_assumes() {
        assert_eq!(
            TABLE.len() % RECORD,
            0,
            "the table must be a whole number of fixed-width records"
        );
        assert!(TABLE.len() / RECORD > 30_000, "the gazetteer looks truncated");
        // Every record ends with its newline, which is what keeps the stride
        // honest — a record one byte short shifts every code after it.
        for index in [0usize, 1, 100, TABLE.len() / RECORD - 1] {
            assert_eq!(TABLE[index * RECORD + RECORD - 1], b'\n', "record {index}");
        }
    }

    #[test]
    fn a_known_code_resolves_to_its_coordinate() {
        let found = lookup("01001").expect("01001 is in the gazetteer");
        assert_eq!(found.zip, "01001");
        assert_eq!(found.lat, "42.062");
        assert_eq!(found.lon, "-72.626");
    }

    /// The ends of the table are where an off-by-one in the binary search
    /// shows up.
    #[test]
    fn the_first_and_last_records_are_reachable() {
        assert!(lookup("00601").is_ok(), "the first record");
        assert!(lookup("99929").is_ok(), "the last record");
    }

    /// The whole table, checked for order and reachability in one pass — a
    /// binary search over an unsorted table finds nothing and says the code
    /// does not exist.
    #[test]
    fn every_record_is_in_order_and_can_be_found() {
        let records = TABLE.len() / RECORD;
        let mut previous = String::new();
        for index in 0..records {
            let at = index * RECORD;
            let code = std::str::from_utf8(&TABLE[at..at + 5]).expect("ASCII codes");
            assert!(
                code > previous.as_str(),
                "record {index} ({code}) is out of order after {previous}"
            );
            previous = code.to_string();
        }
        // Spot-check the search itself across the range rather than 33,791
        // times over.
        for index in (0..records).step_by(997) {
            let at = index * RECORD;
            let code = std::str::from_utf8(&TABLE[at..at + 5]).unwrap();
            assert!(lookup(code).is_ok(), "{code} is in the table but not found");
        }
    }

    #[test]
    fn a_code_is_accepted_however_it_was_typed() {
        assert_eq!(normalise("01001").as_deref(), Some("01001"));
        assert_eq!(normalise("01001-1234").as_deref(), Some("01001"));
        assert_eq!(normalise(" 0 1 0 0 1 ").as_deref(), Some("01001"));
        assert_eq!(normalise("0100").as_deref(), None, "four digits is not a code");
        assert_eq!(normalise("").as_deref(), None);
        assert_eq!(normalise("SW1A 1AA").as_deref(), None);
    }

    #[test]
    fn a_code_outside_the_gazetteer_says_so_usefully() {
        let err = lookup("00000").expect_err("00000 is not a tabulation area");
        assert!(err.contains("00000"));
        assert!(err.contains("neighbouring"), "got: {err}");
    }

    #[test]
    fn padding_is_stripped_without_losing_the_number() {
        assert_eq!(trim_number("+42.062"), "42.062");
        assert_eq!(trim_number("-072.626"), "-72.626");
        assert_eq!(trim_number("+007.500"), "7.500");
        // A coordinate under one degree keeps the zero before its point.
        assert_eq!(trim_number("+000.500"), "0.500");
    }
}
