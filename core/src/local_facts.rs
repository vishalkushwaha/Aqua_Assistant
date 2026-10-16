//! Questions about the time and the date, answered from the computer's clock at once, with no model. They used to go to the chat
//! model, which was told it could not see the clock: a spoken "what time is it" took 3 to 12 seconds to come back as "I can't
//! check that". Only a short question that is clearly about the time or the date is answered here; anything else (a time in another
//! city, "what time does the shop close") goes on to the model as before.

use chrono::NaiveDateTime;

/// The answer to a question about the time or the date, or `None` when the text is not one.
pub fn handle_request(text: &str) -> Option<String> {
    handle_request_at(text, chrono::Local::now().naive_local())
}

/// Lower case, no punctuation, no apostrophes ("What's the time?" is "whats the time").
fn normalise(text: &str) -> String {
    let lowered: String = text
        .to_lowercase()
        .chars()
        .filter(|c| *c != '\'' && *c != '\u{2019}')
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What is asked, with the politeness taken off.
fn core_of(normalised: &str) -> String {
    let mut s = normalised.to_string();
    const LEADING: &[&str] = &["please ", "can you ", "could you ", "would you ", "will you ", "do you know ", "tell me ", "i want to know ", "i would like to know ", "may i know ", "hey aqua ", "okay aqua ", "ok aqua ", "aqua "];
    // " right now" before " now", or "what time is it right now" would lose only its last word.
    const TRAILING: &[&str] = &[" right now", " please", " for me", " now"];
    loop {
        let before = s.clone();
        for p in LEADING {
            if let Some(rest) = s.strip_prefix(p) {
                s = rest.to_string();
            }
        }
        for p in TRAILING {
            if let Some(rest) = s.strip_suffix(p) {
                s = rest.to_string();
            }
        }
        if s == before {
            return s;
        }
    }
}

pub(crate) fn handle_request_at(text: &str, now: NaiveDateTime) -> Option<String> {
    let q = core_of(&normalise(text));
    if q.is_empty() || q.split_whitespace().count() > 9 {
        return None;
    }
    const TIME: &[&str] = &["what time is it", "what is the time", "whats the time", "the time", "current time", "the current time", "what is the current time", "whats the current time", "what time do you have", "what time is it today"];
    const DATE: &[&str] = &["what is the date", "whats the date", "what is todays date", "whats todays date", "todays date", "the date", "current date", "the current date", "what is the current date", "whats the current date", "what date is it", "what date is it today", "what is the date today", "whats the date today", "what is today", "whats today", "the date today"];
    const DAY: &[&str] = &["what day is it", "what day is it today", "what day is today", "which day is it", "which day is it today", "what is the day today", "whats the day today", "what day of the week is it", "what day of the week is it today", "the day today", "the current day", "what is the current day", "whats the current day"];
    const BOTH: &[&str] = &["what is the date and time", "whats the date and time", "the date and time", "date and time", "what is the time and date", "whats the time and date", "the time and date", "what are the date and time"];
    let (time, date, day) = (now.format("%-I:%M %p").to_string(), now.format("%A, %B %-d, %Y").to_string(), now.format("%A").to_string());
    if TIME.contains(&q.as_str()) {
        Some(format!("It's {time}."))
    } else if DATE.contains(&q.as_str()) {
        Some(format!("Today is {date}."))
    } else if DAY.contains(&q.as_str()) {
        Some(format!("Today is {day}."))
    } else if BOTH.contains(&q.as_str()) {
        Some(format!("It's {time} on {date}."))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn saturday_afternoon() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 3).unwrap().and_hms_opt(16, 32, 5).unwrap()
    }

    #[test]
    fn the_time_the_date_and_the_day_are_answered_from_the_clock() {
        let at = saturday_afternoon();
        for q in ["What time is it?", "what's the time", "Tell me the time", "Could you please tell me the current time?", "What time is it right now", "Hey Aqua, what time is it?"] {
            assert_eq!(handle_request_at(q, at).as_deref(), Some("It's 4:32 PM."), "{q}");
        }
        for q in ["What's the date?", "What is today's date", "what date is it today", "Tell me the date please", "What's today"] {
            assert_eq!(handle_request_at(q, at).as_deref(), Some("Today is Saturday, October 3, 2026."), "{q}");
        }
        for q in ["What day is it?", "which day is it today", "What day of the week is it"] {
            assert_eq!(handle_request_at(q, at).as_deref(), Some("Today is Saturday."), "{q}");
        }
        assert_eq!(handle_request_at("What is the date and time?", at).as_deref(), Some("It's 4:32 PM on Saturday, October 3, 2026."));
        // Mornings and midnight read naturally.
        let early = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap().and_hms_opt(0, 7, 0).unwrap();
        assert_eq!(handle_request_at("what time is it", early).as_deref(), Some("It's 12:07 AM."));
    }

    #[test]
    fn anything_else_goes_on_to_the_model() {
        let at = saturday_afternoon();
        for q in [
            "What time is it in Tokyo?",
            "What time does the pharmacy close",
            "Open the clock app",
            "Set a timer for ten minutes",
            "What is the date of the next full moon",
            "Tell me about the history of time",
            "What day is the football match",
            "",
            "time",
            "Remind me at 5 pm to call Bob and also tell me what time it is and what the weather is going to be like tomorrow",
        ] {
            assert_eq!(handle_request_at(q, at), None, "{q:?}");
        }
    }
}
