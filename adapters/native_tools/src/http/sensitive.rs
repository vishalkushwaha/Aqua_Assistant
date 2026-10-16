//! What must not be sent to a search service: a check of the words of a query, before any service is asked.
//!
//! A search query leaves the computer. Whatever the service does with it, it is no longer the person's alone: some keep it, one of those whose terms were read for this
//! (Parallel's customer terms, which the keyless search points to) lets it use what it is sent to train its models and says that personal data needs a separate agreement. The
//! model that writes a query is a small one that copies what is in front of it: a file's path from a task, an address from an email it read, a number from a form. So the words
//! are checked here, by rules and not by a model, and a query that holds such a thing is not sent anywhere; the model is told what it was and asks again without it.
//!
//! Two levels. What no service should be sent **ever**: a secret (a key, a token, a password), a card number, a government number, a private key, and the path of a file on this
//! computer (it names the person and what they keep). What a service that may keep or train on queries should not be sent: also an email address, a telephone number and an address
//! on the person's own network. The second level is for the services whose terms say so, and for none of the others: searching for a company's public email address is an
//! ordinary search.
//!
//! Rules cannot know a name or a diagnosis: this stops what has a shape. It is not a promise that nothing private is in a query.

/// What was found. `what` is a phrase for the model to read: "a card number".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub what: &'static str,
}

impl Found {
    /// What the model is told when the query is not sent.
    pub fn refusal(&self) -> String {
        format!("this search was not made: the query contains {}, and a query leaves this computer. Search again without it (describe what you need to know in general words)", self.what)
    }
}

fn digits_of(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// The Luhn check of a card number.
fn luhn(digits: &str) -> bool {
    let mut sum = 0;
    for (i, c) in digits.chars().rev().enumerate() {
        let Some(mut d) = c.to_digit(10) else { return false };
        if i % 2 == 1 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
    }
    sum % 10 == 0 && !digits.is_empty()
}

/// Groups of digits that can be one number, with the spaces and dashes a card or an ID is written with: `4111 1111 1111 1111`, `1234-5678-9012`.
fn number_groups(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut last_was_digit = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
            last_was_digit = true;
        } else if (c == ' ' || c == '-') && last_was_digit {
            cur.push(c);
            last_was_digit = false;
        } else {
            if !cur.is_empty() {
                out.push(cur.trim_end_matches([' ', '-']).to_string());
            }
            cur = String::new();
            last_was_digit = false;
        }
    }
    if !cur.is_empty() {
        out.push(cur.trim_end_matches([' ', '-']).to_string());
    }
    out
}

fn patterns() -> &'static (regex::Regex, regex::Regex, regex::Regex, regex::Regex, regex::Regex, regex::Regex) {
    static P: std::sync::OnceLock<(regex::Regex, regex::Regex, regex::Regex, regex::Regex, regex::Regex, regex::Regex)> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        (
            // A path on this computer: a drive and a user folder, a home folder.
            regex::Regex::new(r"(?i)\b[a-z]:[\\/]+(users|documents and settings)[\\/]+[^\\/\s]+|(^|[\s(\[\x22'])(/home|/users)/[a-z0-9._-]+/").expect("valid"),
            // PAN (the Indian tax number), the US social security number.
            regex::Regex::new(r"\b[A-Z]{5}[0-9]{4}[A-Z]\b|\b[0-9]{3}-[0-9]{2}-[0-9]{4}\b").expect("valid"),
            regex::Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").expect("valid"),
            // An email address.
            regex::Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+").expect("valid"),
            // An address on the person's own network.
            regex::Regex::new(r"\b(10\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|172\.(1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3})\b").expect("valid"),
            // "password is ...", "my pin: ...": a word that names a secret, and what follows it.
            regex::Regex::new(r"(?i)\b(password|passwd|passphrase|pin code|my pin|otp|one[- ]time (code|password)|cvv|secret key|api key|access token)\s*(is|=|:)\s*\S+").expect("valid"),
        )
    })
}

/// The first thing in `query` that must not be sent. `strict` is for a service that may keep or train on what it is sent.
pub fn find(query: &str, strict: bool) -> Option<Found> {
    let (path, id, key_block, email, network, password) = patterns();
    // A secret: anything the host's own redactor would mask (keys, tokens, `password=...`, private key blocks).
    if aqua_tool_host::Redactor::new().redact(query) != query {
        return Some(Found { what: "a secret (a key, a token or a password)" });
    }
    if key_block.is_match(query) {
        return Some(Found { what: "a private key" });
    }
    if password.is_match(query) {
        return Some(Found { what: "a password, a PIN or a one-time code" });
    }
    for group in number_groups(query) {
        let digits = digits_of(&group);
        // A card number: 13 to 19 digits that pass the check, written in groups or not.
        if (13..=19).contains(&digits.len()) && luhn(&digits) {
            return Some(Found { what: "a card number" });
        }
        // An Aadhaar number: twelve digits, in groups of four or not at all (a 12-digit number written otherwise is something else).
        let parts: Vec<&str> = group.split([' ', '-']).collect();
        let in_fours = parts.len() == 3 && parts.iter().all(|p| p.len() == 4);
        if digits.len() == 12 && (in_fours || group.len() == 12) && !digits.starts_with(['0', '1']) {
            return Some(Found { what: "a government identity number" });
        }
    }
    if id.is_match(query) {
        return Some(Found { what: "a government identity or tax number" });
    }
    if path.is_match(query) {
        return Some(Found { what: "the path of a file on this computer" });
    }
    if strict {
        if email.is_match(query) {
            return Some(Found { what: "an email address" });
        }
        if network.is_match(query) {
            return Some(Found { what: "an address on the person's own network" });
        }
        // A telephone number: ten digits or more in one run of digits, spaces, dashes and brackets, with a plus or a leading zero or a grouping.
        for group in number_groups(query) {
            let digits = digits_of(&group);
            if (10..=15).contains(&digits.len()) && (group.contains('-') || group.contains(' ') || group.starts_with('0')) {
                return Some(Found { what: "a telephone number" });
            }
        }
        if query.contains('+') && regex::Regex::new(r"\+\d[\d\s().-]{8,}\d").expect("valid").is_match(query) {
            return Some(Found { what: "a telephone number" });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_has_the_shape_of_a_secret_a_card_an_id_or_a_file_on_this_computer_is_found_at_the_level_for_every_service() {
        for (q, what) in [
            ("is sk-proj-abcdefghijklmnopqrstuvwx1234567890 a valid key", "a secret"),
            ("why does AKIAIOSFODNN7EXAMPLE not work", "a secret"),
            ("my password is hunter2secret99 how do I change it", "password"),
            ("what is wrong with this: -----BEGIN RSA PRIVATE KEY----- MIIE", "a private key"),
            ("is 4111 1111 1111 1111 a valid visa number", "a card number"),
            ("4111111111111111 declined", "a card number"),
            ("track my aadhaar 2345 6789 0123", "a government identity number"),
            ("update PAN ABCDE1234F details", "a government identity or tax number"),
            ("ssn 123-45-6789 lookup", "a government identity or tax number"),
            (r"what is the file C:\Users\lenovo\Documents\tax return 2026.pdf", "the path of a file on this computer"),
            ("open /home/priya/projects/aqua/secret.txt in vim", "the path of a file on this computer"),
            ("my pin is 4821 what now", "a password, a PIN or a one-time code"),
        ] {
            let f = find(q, false).unwrap_or_else(|| panic!("not found: {q}"));
            assert!(f.what.contains(what), "{q}: {}", f.what);
            assert!(f.refusal().starts_with("this search was not made: the query contains") && f.refusal().contains("Search again without it"));
        }
    }

    #[test]
    fn ordinary_queries_numbers_and_public_addresses_are_not_stopped_at_either_level() {
        for q in [
            "capital of australia",
            "rust 1.90 release notes",
            "error 0x80070005 access denied windows 11",
            "what is 192.168.1.1 router login page default",
            "ISBN 9780140449136 the odyssey",
            "iphone 17 price in india 2026 256gb",
            "the 2024 census counted 1,428,627,663 people",
            "tesla stock price today",
            "how to write a password policy for a small company",
            "what is a one-time password and how does it work",
            "a phone number format in the uk",
            "hotel near 221 baker street london",
            "meeting at 10.30 on 12-10-2026",
        ] {
            assert_eq!(find(q, false), None, "{q}");
        }
        // The path of a public thing, and a folder name that is only a word.
        assert_eq!(find("what is the users folder in windows", false), None);
        assert_eq!(find("c:\\windows\\system32 what is it", false), None, "a system folder names nobody");
        // A number that fails the check is not a card.
        assert_eq!(find("order number 4111 1111 1111 1112 status", false), None);
    }

    #[test]
    fn a_service_that_may_keep_or_train_on_queries_is_also_not_sent_an_email_a_phone_number_or_a_private_address() {
        for (q, what) in [
            ("who is priya.sharma@example.com", "an email address"),
            ("find the owner of 192.168.0.12 on my network", "an address on the person's own network"),
            ("call back +91 98765 43210 who is it", "a telephone number"),
            ("whose number is 98765-43210", "a telephone number"),
            ("is 020 7946 0958 a real number", "a telephone number"),
        ] {
            assert_eq!(find(q, false), None, "{q}: fine for a service that does not keep queries");
            let f = find(q, true).unwrap_or_else(|| panic!("not found: {q}"));
            assert!(f.what.contains(what), "{q}: {}", f.what);
        }
        // Still not stopped: an ordinary search with numbers in it.
        for q in ["capital of australia", "rust 1.90 release notes", "iphone 17 price in india 2026 256gb", "the 2024 census counted 1,428,627,663 people", "meeting at 10.30 on 12-10-2026"] {
            assert_eq!(find(q, true), None, "{q}");
        }
    }

    #[test]
    fn the_luhn_check_and_the_grouping_of_numbers() {
        assert!(luhn("4111111111111111") && luhn("5500005555555559") && !luhn("4111111111111112") && !luhn(""));
        assert_eq!(number_groups("a 4111 1111 1111 1111 b 22-33 c 9"), vec!["4111 1111 1111 1111", "22-33", "9"]);
    }
}
