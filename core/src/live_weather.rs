//! A plain question about the weather, answered from a weather service in about a second, with no browser and no model.
//!
//! Weather is the most common request that needs the internet, and it went through the whole agent: a Google search in the browser, a
//! snapshot of the page, and a round of the 8B model to write one sentence from about 3,000 tokens of page text. In the log those
//! took 156 to 343 seconds each. The same facts come from one request to wttr.in (on the default allow list of the web tools), which
//! answers in 0.7 seconds with the temperature, the conditions, the wind and the day's high, low and chance of rain.
//!
//! Only a short question that is clearly about the weather of today or tomorrow is answered here, and only from the service's own
//! numbers; anything else (a week's forecast, "will it rain", a comparison of two cities, a question that merely mentions the weather)
//! goes on to the agent as before. So does every failure: no network, an unknown place, an answer that is not what was expected.
//! The call goes through the same guarded tool host as every other tool (`web.fetch`: the network policy, the allow list, the audit
//! record), and asks for the service's compact format (3 KB instead of 39 KB). It is made as the API caller, not the voice caller: the
//! host refuses every network call to a voice caller ("voice can only read"), a rule made for what a model might choose after an overheard
//! sentence. This one is not chosen by anyone: the address is fixed to the one service on the default allow list, the place in it is
//! letters and spaces from a strict grammar, nothing is sent but that name, and nothing is changed. `AQUA_WEATHER_DIRECT=off` turns it off.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Which day is asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Day {
    Today,
    Tomorrow,
}

/// What a weather question asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ask {
    /// The place in the person's words ("new delhi"); `None` is wherever the service finds this computer's address to be.
    pub place: Option<String>,
    pub day: Day,
}

/// A fetched answer is reused as-is for this long, so that asking again does not ask the service again.
const KEEP: Duration = Duration::from_secs(15 * 60);
/// After that, the same answer is still used when the service cannot be reached, instead of starting a browser search.
const STALE: Duration = Duration::from_secs(3 * 60 * 60);
/// After a failure to reach the service nothing is tried for this long, so that a request does not wait for the same time-out each time.
const COOL_OFF: Duration = Duration::from_secs(60);
/// How long one request to the service may take before the agent is asked instead (it answers in 0.7 s; the tool's own limit is 30 s).
const TIMEOUT: Duration = Duration::from_secs(6);

/// `AQUA_WEATHER_DIRECT=off` turns this shortcut off: every weather question then goes through the agent.
fn enabled() -> bool {
    !std::env::var("AQUA_WEATHER_DIRECT").is_ok_and(|v| v.eq_ignore_ascii_case("off") || v == "0")
}

/// The answer to a question about the weather, or `None` when the text is not one or the answer could not be had (the agent then takes it).
pub fn handle_request(text: &str) -> Option<String> {
    if !enabled() {
        return None;
    }
    let ask = ask_of(text)?;
    let host = crate::agent_tools::host()?.clone();
    handle_ask(&ask, |url| {
        // On its own thread so that a service that does not answer costs `TIMEOUT`, not the tool's 30 seconds (and its retry).
        let (tx, rx) = std::sync::mpsc::channel();
        let (host, url) = (host.clone(), url.to_string());
        if std::thread::Builder::new().name("aqua-weather".into()).spawn(move || drop(tx.send(fetch_through_host(&host, &url)))).is_err() {
            return Fetched::Down;
        }
        rx.recv_timeout(TIMEOUT).unwrap_or(Fetched::Down)
    })
}

/// The place a weather question asks about, if the text is one this module answers (`Some(None)` is "where this computer is").
pub fn place_asked_about(text: &str) -> Option<Option<String>> {
    ask_of(text).map(|a| a.place)
}

/// What a request to the service came to.
enum Fetched {
    Data(Value),
    /// The service answered and does not know this place (or sent something unexpected). That is the person's word, not an outage: the
    /// agent takes the question, and the next one is not held back.
    Unknown,
    /// No answer, a time-out, or an error on the service's side: nothing is tried for a minute.
    Down,
}

fn handle_ask(ask: &Ask, fetch: impl Fn(&str) -> Fetched) -> Option<String> {
    let url = url_for(ask.place.as_deref());
    if let Some((data, age)) = cached_with_age(&url, KEEP) {
        return compose(ask, &data).map(|s| as_of(&s, age));
    }
    if cooling_off() {
        return last_or_unreachable(ask, &url);
    }
    let data = match fetch(&url) {
        Fetched::Data(data) => {
            remember(&url, &data);
            data
        }
        Fetched::Unknown => return None,
        Fetched::Down => {
            fail();
            return last_or_unreachable(ask, &url);
        }
    };
    compose(ask, &data)
}

/// The last weather for this place, or a short refusal. Never `None`: that sends the question to the agent, which searches the
/// web and takes minutes, and the next identical question does it again.
fn last_or_unreachable(ask: &Ask, url: &str) -> Option<String> {
    if let Some((data, age)) = cached_with_age(url, STALE) {
        return compose(ask, &data).map(|s| {
            let said = as_of(&s, age);
            // "When I last checked, in Pune it is ...": the sentence goes on from the comma.
            let mut chars = said.chars();
            let rest = chars.next().map(|c| c.to_lowercase().collect::<String>() + chars.as_str()).unwrap_or_default();
            format!("When I last checked, {rest}")
        });
    }
    Some("I couldn't check the weather just now, so I won't search the web for it. Ask again in a minute.".into())
}

fn url_for(place: Option<&str>) -> String {
    format!("https://wttr.in/{}?format=j2", place.map(|p| p.split_whitespace().collect::<Vec<_>>().join("+")).unwrap_or_default())
}

/// The request to the service, through the tool host (as the API caller, see the top of this file).
fn fetch_through_host(host: &aqua_tool_host::ToolHost, url: &str) -> Fetched {
    use aqua_tool_host::{Caller, RequestContext, ToolRequest};
    let result = host.call_blocking(ToolRequest { tool: "web.fetch".into(), arguments: json!({ "url": url, "max_chars": 20_000 }), context: RequestContext::new(Caller::Api) });
    let fetched = classify(result.ok, result.data["status"].as_u64(), &result.text);
    if !matches!(fetched, Fetched::Data(_)) {
        // The first line is the tool's label for untrusted text; the one that matters is the status line after it.
        tracing::info!("weather service not used: {}", result.text.lines().find(|l| !l.trim().is_empty() && !l.starts_with("<untrusted")).unwrap_or("no answer"));
    }
    fetched
}

/// A tool result as a fetch: the data, a place the service does not know, or an outage (the tool failed, a time-out, a 5xx, a 429).
/// The service says "location not found" with HTTP **500** (found by asking it for a place that does not exist: `Xqzvbtl`), so the words in
/// the body count as well as a 4xx: a mis-heard name is not an outage and must not rest the service.
fn classify(tool_ok: bool, status: Option<u64>, text: &str) -> Fetched {
    let lower = text.to_ascii_lowercase();
    let unknown_place = lower.contains("location not found") || lower.contains("unknown location");
    match (tool_ok, status) {
        (true, Some(200)) => json_in(text).map_or(Fetched::Unknown, Fetched::Data),
        (true, Some(s)) if unknown_place || ((400..500).contains(&s) && s != 408 && s != 429) => Fetched::Unknown,
        _ => Fetched::Down,
    }
}

/// The first JSON object in the text of a result (the tool puts the address and a label around it).
fn json_in(text: &str) -> Option<Value> {
    let from = text.find('{')?;
    let data = serde_json::Deserializer::from_str(&text[from..]).into_iter::<Value>().next()?.ok()?;
    data.get("current_condition")?.get(0)?;
    Some(data)
}

// ---- What is asked -----------------------------------------------------------------------------------------------------------

/// Words that carry no place: the question's own words, politeness, time words, prepositions.
const FILLER: &[&str] = &[
    "moment", "time", "minute", "again", "just", "so", "um", "uh", "hmm", "well", "yeah", "yes", "hi", "hello", "to",
    "what", "whats", "how", "hows", "is", "are", "the", "a", "an", "of", "in", "at", "on", "for", "near", "around", "like", "about", "tell", "me", "us", "give", "show", "get", "check", "find", "look", "up", "please", "can", "could", "would", "you", "i", "want", "to", "know",
    "hey", "aqua", "okay", "ok", "today", "todays", "now", "right", "currently", "current", "report", "update", "conditions", "condition", "outside", "forecast", "it", "its", "there", "weather", "wather", "wheather", "temperature", "degrees", "going", "be", "doing", "do", "s", "latest", "live",
];

/// Words that make a question about something else, or about more than today and tomorrow, or about something that needs more than the
/// numbers: left to the agent.
const NOT_FOR_THIS: &[&str] = &[
    "later", "soon",
    "forecast", "week", "weekend", "weekly", "next", "yesterday", "last", "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday", "hourly", "tonight", "morning", "afternoon", "evening", "night", "month", "monthly", "season", "rain", "raining", "rainy", "snow", "snowing", "umbrella", "storm",
    "will", "wont", "should", "wear", "jacket", "sweater", "history", "historical", "average", "climate", "compare", "versus", "vs", "than", "and", "or", "app", "apps", "api", "widget", "python", "rust", "code", "install", "build", "make", "write", "create", "fix", "set", "change", "add",
    "news", "map", "radar", "satellite", "alert", "alerts", "warning", "warnings", "pollen", "air", "quality", "aqi", "uv", "sunrise", "sunset", "moon", "tide", "tides", "wind", "humidity",
];

/// Words that say the place is this computer's (the service then goes by the address), or that the "place" is not a place at all.
const MY_PLACE: &[&str] = &["my", "our", "here", "local", "location", "city", "area", "town", "place", "home", "nearby", "locally", "me"];
const NOT_A_PLACE: &[&str] = &[
    "cpu", "gpu", "processor", "disk", "body", "oven", "room", "water", "coolant", "laptop", "computer", "pc", "battery", "fever", "sensor", "core", "cores", "chip", "windows", "chrome", "google", "screen", "desktop", "phone", "taskbar", "docs", "documentation", "server", "file", "folder",
    "this", "that", "your", "yours", "his", "her", "their", "something", "anything", "everything", "one", "two", "three", "mars", "moon", "sun", "jupiter", "venus", "space", "universe",
];

fn normalise(text: &str) -> Vec<String> {
    text.to_lowercase()
        .chars()
        .filter(|c| *c != '\'' && *c != '\u{2019}')
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Words that start a phrase about a place or a time ("in Pune", "at the moment", "for Los Angeles").
const PREPOSITIONS: &[&str] = &["in", "at", "for", "near", "around", "of", "on"];

/// What is left of the question once the words that carry no place are taken out, when it names one place at most. "Weather in Pune at the
/// moment" leaves "pune" (the phrase "at the moment" says nothing about a place; before, its leftover word was taken for part of the name,
/// and the service was asked for "pune moment"). "Weather in Pune at the airport" leaves two phrases and is not a plain question.
fn words_of_the_place(words: &[String]) -> Option<Vec<&str>> {
    let mut phrases: Vec<Vec<&str>> = vec![Vec::new()];
    for w in words {
        if PREPOSITIONS.contains(&w.as_str()) {
            phrases.push(Vec::new());
        } else if !FILLER.contains(&w.as_str()) && !["tomorrow", "tomorrows", "temp"].contains(&w.as_str()) {
            phrases.last_mut()?.push(w.as_str());
        }
    }
    let mut named = phrases.into_iter().filter(|p| !p.is_empty());
    let first = named.next();
    if named.next().is_some() {
        return None;
    }
    Some(first.unwrap_or_default())
}

/// The weather question in `text`, or `None` when it is not a plain one.
pub(crate) fn ask_of(text: &str) -> Option<Ask> {
    let words = normalise(text);
    if words.is_empty() || words.len() > 14 {
        return None;
    }
    let has = |w: &str| words.iter().any(|x| x == w);
    // "wather" and "wheather" are the mis-hearings that otherwise fall through to a browser search.
    let weather = has("weather") || has("wather") || has("wheather");
    let temperature = has("temperature") || has("temp");
    if !weather && !temperature {
        return None;
    }
    if words.iter().any(|w| NOT_FOR_THIS.contains(&w.as_str())) {
        return None;
    }
    let day = if has("tomorrow") || has("tomorrows") { Day::Tomorrow } else { Day::Today };
    let rest = words_of_the_place(&words)?;
    // Something about "my" place, or this computer's: the service goes by the address.
    let mine = rest.iter().all(|w| MY_PLACE.contains(w));
    if !mine && rest.iter().any(|w| MY_PLACE.contains(w)) {
        return None;
    }
    let place = if rest.is_empty() || mine {
        None
    } else {
        if rest.len() > 4 || rest.iter().any(|w| NOT_A_PLACE.contains(w) || w.chars().any(|c| c.is_ascii_digit()) || w.chars().count() < 2 || w.chars().count() > 20) {
            return None;
        }
        Some(rest.join(" "))
    };
    // "Temperature" alone is as likely to be a processor's: it needs a place of its own to count as weather.
    if !weather && place.is_none() {
        return None;
    }
    Some(Ask { place, day })
}

// ---- What is said ------------------------------------------------------------------------------------------------------------

fn number(value: &Value) -> Option<i64> {
    value.as_str().and_then(|s| s.trim().parse::<f64>().ok()).or_else(|| value.as_f64()).map(|f| f.round() as i64)
}

fn title_case(text: &str) -> String {
    text.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// "Pune, India": the place as asked, with its country when the person did not say it (the service may find another place of the same
/// name), and the state for the countries that have many places of one name.
fn label(ask: &Ask, data: &Value) -> String {
    let area = data["nearest_area"].get(0);
    let field = |name: &str| area.and_then(|a| a[name].get(0)).and_then(|v| v["value"].as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let Some(asked) = ask.place.as_deref() else {
        // By address: the service's own name for the area, with the country.
        return [field("areaName"), field("country")].into_iter().flatten().collect::<Vec<_>>().join(", ");
    };
    let mut parts = vec![title_case(asked)];
    let lower = asked.to_lowercase();
    let country = field("country").map(|c| match c.as_str() {
        "United States of America" => "United States".to_string(),
        _ => c,
    });
    if let Some(region) = field("region") {
        if ["United States", "Canada", "Australia"].contains(&country.as_deref().unwrap_or("")) && !lower.contains(&region.to_lowercase()) {
            parts.push(region);
        }
    }
    if let Some(country) = country {
        if !lower.contains(&country.to_lowercase()) {
            parts.push(country);
        }
    }
    parts.join(", ")
}

fn direction(code: &str) -> &'static str {
    match code.trim() {
        "N" => "north",
        "NNE" => "north-northeast",
        "NE" => "northeast",
        "ENE" => "east-northeast",
        "E" => "east",
        "ESE" => "east-southeast",
        "SE" => "southeast",
        "SSE" => "south-southeast",
        "S" => "south",
        "SSW" => "south-southwest",
        "SW" => "southwest",
        "WSW" => "west-southwest",
        "W" => "west",
        "WNW" => "west-northwest",
        "NW" => "northwest",
        "NNW" => "north-northwest",
        _ => "",
    }
}

fn description(entry: &Value) -> Option<String> {
    entry["weatherDesc"].get(0)?["value"].as_str().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty())
}

/// The spoken answer from the service's data, or `None` when the numbers it needs are not there.
pub(crate) fn compose(ask: &Ask, data: &Value) -> Option<String> {
    let place = label(ask, data);
    let place = if place.is_empty() { "your area".to_string() } else { place };
    match ask.day {
        Day::Today => {
            let now = data["current_condition"].get(0)?;
            let temp = number(&now["temp_C"])?;
            let condition = description(now);
            let today = data["weather"].get(0);
            let mut out = match &condition {
                Some(c) => format!("In {place} it is {temp} degrees Celsius and {c} right now"),
                None => format!("In {place} it is {temp} degrees Celsius right now"),
            };
            if let Some(feels) = number(&now["FeelsLikeC"]).filter(|f| (f - temp).abs() >= 2) {
                out.push_str(&format!(", and it feels like {feels}"));
            }
            out.push('.');
            let humidity = number(&now["humidity"]);
            let wind = number(&now["windspeedKmph"]);
            match (humidity, wind) {
                (Some(h), Some(w)) if w >= 6 => {
                    let from = direction(now["winddir16Point"].as_str().unwrap_or(""));
                    let from = if from.is_empty() { String::new() } else { format!(" from the {from}") };
                    out.push_str(&format!(" Humidity is {h} percent and the wind is {w} kilometers per hour{from}."));
                }
                (Some(h), Some(_)) => out.push_str(&format!(" Humidity is {h} percent, with hardly any wind.")),
                (Some(h), None) => out.push_str(&format!(" Humidity is {h} percent.")),
                _ => {}
            }
            if let Some(day) = today {
                if let (Some(high), Some(low)) = (number(&day["maxtempC"]), number(&day["mintempC"])) {
                    out.push_str(&format!(" Today's high is {high} degrees and the low is {low}"));
                    out.push_str(&rain_clause(day));
                    out.push('.');
                }
            }
            Some(out)
        }
        Day::Tomorrow => {
            let day = data["weather"].get(1)?;
            let (high, low) = (number(&day["maxtempC"])?, number(&day["mintempC"])?);
            // The conditions at midday stand for the day.
            let midday = day["hourly"].as_array().and_then(|h| h.iter().find(|e| e["time"].as_str() == Some("1200")).or_else(|| h.get(h.len() / 2)));
            let condition = midday.and_then(description).map(|c| format!(", {c}")).unwrap_or_default();
            Some(format!("Tomorrow in {place} it will be between {low} and {high} degrees Celsius{condition}{}.", rain_clause(day)))
        }
    }
}

/// ", with up to a 30 percent chance of rain" when the day's highest chance is 30 or more.
fn rain_clause(day: &Value) -> String {
    let chance = day["hourly"].as_array().map(|h| h.iter().filter_map(|e| number(&e["chanceofrain"])).max().unwrap_or(0)).unwrap_or(0);
    if chance >= 30 {
        format!(", with up to a {chance} percent chance of rain")
    } else {
        String::new()
    }
}

// ---- Keeping and failing -----------------------------------------------------------------------------------------------------

struct Memory {
    answers: HashMap<String, (Instant, Value)>,
    failed_at: Option<Instant>,
}

fn memory() -> std::sync::MutexGuard<'static, Memory> {
    static MEMORY: std::sync::OnceLock<Mutex<Memory>> = std::sync::OnceLock::new();
    MEMORY.get_or_init(|| Mutex::new(Memory { answers: HashMap::new(), failed_at: None })).lock().unwrap_or_else(PoisonError::into_inner)
}

/// What was fetched for this address in the last few minutes, and how long ago.
fn cached_with_age(url: &str, max_age: Duration) -> Option<(Value, Duration)> {
    let mut m = memory();
    m.answers.retain(|_, (at, _)| at.elapsed() < STALE);
    m.answers.get(url).filter(|(at, _)| at.elapsed() < max_age).map(|(at, data)| (data.clone(), at.elapsed()))
}

/// An answer from the cache that is older than this does not say "right now" (card R6-18).
const FRESH_FOR: Duration = Duration::from_secs(2 * 60);

/// How long ago, in words: "5 minutes ago", "1 hour ago".
fn ago(age: Duration) -> String {
    let minutes = age.as_secs() / 60;
    if minutes < 60 {
        format!("{minutes} minute{} ago", if minutes == 1 { "" } else { "s" })
    } else {
        let hours = minutes / 60;
        format!("{hours} hour{} ago", if hours == 1 { "" } else { "s" })
    }
}

/// The answer for the weather now, said as of when it was fetched when that was a while ago: the cache keeps an answer for 15 minutes (and, when the service is down, for 3 hours), and
/// "right now" for a quarter of an hour old is not true. A forecast (tomorrow) does not say "right now" and is left as it is.
fn as_of(answer: &str, age: Duration) -> String {
    if age < FRESH_FOR {
        return answer.to_string();
    }
    answer.replacen(" right now", &format!(", as of {}", ago(age)), 1)
}

fn remember(url: &str, data: &Value) {
    let mut m = memory();
    m.failed_at = None;
    // The weather of a few places is all that is ever asked for.
    if m.answers.len() >= 16 {
        m.answers.clear();
    }
    m.answers.insert(url.to_string(), (Instant::now(), data.clone()));
}

fn fail() {
    memory().failed_at = Some(Instant::now());
}

fn cooling_off() -> bool {
    memory().failed_at.is_some_and(|at| at.elapsed() < COOL_OFF)
}

#[cfg(test)]
fn forget_everything() {
    let mut m = memory();
    m.answers.clear();
    m.failed_at = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The memory is shared by the whole process, so the tests that touch it take turns.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn ask(text: &str) -> Option<(Option<String>, Day)> {
        ask_of(text).map(|a| (a.place, a.day))
    }

    fn today(place: &str) -> Option<(Option<String>, Day)> {
        Some((Some(place.to_string()), Day::Today))
    }

    /// The shape of a wttr.in answer (`?format=j1`), cut down to what is read, with real values for Pune.
    fn pune() -> Value {
        json!({
            "current_condition": [{ "temp_C": "24", "FeelsLikeC": "25", "humidity": "61", "windspeedKmph": "4", "winddir16Point": "W", "weatherDesc": [{ "value": "Cloudy " }] }],
            "nearest_area": [{ "areaName": [{ "value": "Bhambhurda" }], "region": [{ "value": "Maharashtra" }], "country": [{ "value": "India" }] }],
            "weather": [
                { "maxtempC": "32", "mintempC": "22", "hourly": [
                    { "time": "0", "chanceofrain": "15", "weatherDesc": [{ "value": "Clear" }] }, { "time": "1200", "chanceofrain": "1", "weatherDesc": [{ "value": "Partly cloudy" }] }, { "time": "1800", "chanceofrain": "13", "weatherDesc": [{ "value": "Cloudy" }] } ] },
                { "maxtempC": "31", "mintempC": "22", "hourly": [
                    { "time": "900", "chanceofrain": "40", "weatherDesc": [{ "value": "Light rain shower" }] }, { "time": "1200", "chanceofrain": "62", "weatherDesc": [{ "value": "Light rain shower" }] } ] }
            ]
        })
    }

    #[test]
    fn a_follow_up_written_out_by_the_conversation_is_understood_and_tells_which_place_it_asked_about() {
        // "and tomorrow?" after a weather answer becomes one of these (see `conversation::resolve_reference`).
        assert_eq!(ask("what's the weather tomorrow in Pune"), Some((Some("pune".into()), Day::Tomorrow)));
        assert_eq!(ask("what's the weather today in New Delhi"), Some((Some("new delhi".into()), Day::Today)));
        assert_eq!(ask("what's the weather tomorrow"), Some((None, Day::Tomorrow)), "no place: where this computer is");
        assert_eq!(place_asked_about("what's the weather in Pune?"), Some(Some("pune".into())));
        assert_eq!(place_asked_about("what's the weather"), Some(None));
        assert_eq!(place_asked_about("tell me a joke"), None, "not a weather question: nothing to remember");
    }

    #[test]
    fn plain_weather_questions_are_understood_with_or_without_a_place() {
        for (said, place) in [
            ("What's the weather in Pune?", "pune"),
            ("what is the weather like in pune today", "pune"),
            ("Hey Aqua, what's the weather in Pune?", "pune"),
            ("weather in Pune", "pune"),
            ("Pune weather", "pune"),
            ("pune weather today", "pune"),
            ("weather Pune", "pune"),
            ("How's the weather in New Delhi right now?", "new delhi"),
            ("Tell me the weather in Mumbai please", "mumbai"),
            ("What is the temperature in Bikaner?", "bikaner"),
            ("temperature in Delhi", "delhi"),
            ("Can you check the weather for San Francisco?", "san francisco"),
            ("weather report for Los Angeles", "los angeles"),
            ("What's the current weather in London?", "london"),
            ("weather in St. Louis", "st louis"),
        ] {
            assert_eq!(ask(said), today(place), "{said:?}");
        }
        for said in ["What's the weather?", "How's the weather today?", "Tell me today's weather.", "Today is weather. Tell me today is weather.", "weather", "what's the weather like outside", "weather here", "what's the weather in my city", "today wather", "whats the wheather"] {
            assert_eq!(ask(said), Some((None, Day::Today)), "{said:?}: by the address of this computer");
        }
        // Found in a review: the leftover words of a phrase about the time were taken for part of the place ("pune moment").
        for (said, place) in [
            ("What's the weather in Pune at the moment?", "pune"),
            ("what's the weather like at the moment in Pune", "pune"),
            ("weather in Pune right now please", "pune"),
            ("hello, what is the weather going to be in Pune", "pune"),
            ("Pune weather again", "pune"),
            ("weather for today in Pune", "pune"),
            ("pune weather in a minute", "pune"),
        ] {
            assert_eq!(ask(said), today(place), "{said:?}");
        }
        assert_eq!(ask("What's the weather in Pune tomorrow?"), Some((Some("pune".into()), Day::Tomorrow)));
        assert_eq!(ask("tomorrow's weather in Goa"), Some((Some("goa".into()), Day::Tomorrow)));
    }

    #[test]
    fn anything_else_is_left_to_the_agent() {
        for said in [
            // more than today and tomorrow, or more than the numbers
            "What's the weather forecast for the week in Pune?", "weather this weekend", "weather on Friday in Mumbai", "what will the weather be tonight", "will it rain in London",
            "is it raining in Pune", "do I need an umbrella in Delhi", "what should I wear, check the weather", "weather in Pune and Mumbai", "compare the weather in Pune and Delhi",
            "weather in Pune versus Mumbai", "weather in Pune at the airport", "Pune weather in Mumbai", "weather in Pune in India", "weather in Pune later", "weather soon in Pune", "what's the humidity in Pune", "what's the wind speed in Pune", "air quality in Pune weather", "weather radar for Pune",
            // not about the weather of a place
            "what's the CPU temperature", "temperature", "what is the temperature", "what's the body temperature of a dog", "what temperature should the oven be",
            "how do I build a weather app in Python", "fix the weather widget on my desktop", "write code that fetches the weather", "weather in the Python docs",
            "what's the weather on Mars", "open the weather app", "weather in my pocket", "tell me the news and the weather",
            // not a question about the weather at all
            "what time is it", "open Notepad", "the weather is nice today and I am going out", "hello", "",
        ] {
            assert_eq!(ask(said), None, "{said:?}");
        }
        assert_eq!(ask("What is the weather like in a city with a very long name that nobody could possibly ask about today"), None, "too long to be a plain question");
    }

    #[test]
    fn the_address_is_made_from_the_place_in_the_persons_words() {
        assert_eq!(url_for(Some("new delhi")), "https://wttr.in/new+delhi?format=j2");
        assert_eq!(url_for(Some("pune")), "https://wttr.in/pune?format=j2");
        assert_eq!(url_for(None), "https://wttr.in/?format=j2");
    }

    #[test]
    fn the_answer_for_today_gives_the_numbers_the_service_gave_and_names_the_place_asked() {
        let said = compose(&Ask { place: Some("pune".into()), day: Day::Today }, &pune()).unwrap();
        assert_eq!(
            said,
            "In Pune, India it is 24 degrees Celsius and cloudy right now. Humidity is 61 percent, with hardly any wind. Today's high is 32 degrees and the low is 22."
        );
        // A noticeable difference between the temperature and how it feels, a wind, and a good chance of rain are said.
        let mut windy = pune();
        windy["current_condition"][0]["temp_C"] = json!("31");
        windy["current_condition"][0]["FeelsLikeC"] = json!("36");
        windy["current_condition"][0]["windspeedKmph"] = json!("14");
        windy["weather"][0]["hourly"][2]["chanceofrain"] = json!("45");
        assert_eq!(
            compose(&Ask { place: Some("new delhi".into()), day: Day::Today }, &windy).unwrap(),
            "In New Delhi, India it is 31 degrees Celsius and cloudy right now, and it feels like 36. Humidity is 61 percent and the wind is 14 kilometers per hour from the west. Today's high is 32 degrees and the low is 22, with up to a 45 percent chance of rain."
        );
    }

    #[test]
    fn the_answer_for_tomorrow_uses_the_second_day_and_the_conditions_at_midday() {
        let said = compose(&Ask { place: Some("pune".into()), day: Day::Tomorrow }, &pune()).unwrap();
        assert_eq!(said, "Tomorrow in Pune, India it will be between 22 and 31 degrees Celsius, light rain shower, with up to a 62 percent chance of rain.");
        let mut one_day = pune();
        one_day["weather"].as_array_mut().unwrap().pop();
        assert_eq!(compose(&Ask { place: Some("pune".into()), day: Day::Tomorrow }, &one_day), None, "no second day: no answer rather than a guess");
    }

    #[test]
    fn the_place_is_the_one_asked_with_its_country_unless_that_was_said_and_its_state_where_names_repeat() {
        let in_ = |asked: &str, region: &str, country: &str| {
            let mut d = pune();
            d["nearest_area"][0]["region"][0]["value"] = json!(region);
            d["nearest_area"][0]["country"][0]["value"] = json!(country);
            label(&Ask { place: Some(asked.into()), day: Day::Today }, &d)
        };
        assert_eq!(in_("pune", "Maharashtra", "India"), "Pune, India", "the service's own name for the spot (a village near Pune) is never used");
        assert_eq!(in_("new delhi", "Delhi", "India"), "New Delhi, India");
        assert_eq!(in_("springfield", "Illinois", "United States of America"), "Springfield, Illinois, United States");
        assert_eq!(in_("london", "City of London, Greater London", "United Kingdom"), "London, United Kingdom", "no region where names do not repeat");
        assert_eq!(in_("india", "Maharashtra", "India"), "India", "already said");
        // By the address: the service's name for the area, with the country.
        assert_eq!(label(&Ask { place: None, day: Day::Today }, &pune()), "Bhambhurda, India");
    }

    /// Card R6-18: the weather is cached for 15 minutes, and an answer from the cache that is not new says when it was fetched instead of "right now".
    #[test]
    fn an_answer_from_the_cache_says_how_long_ago_it_was_fetched_and_a_new_one_says_right_now() {
        let _only = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        forget_everything();
        let ask = Ask { place: Some("pune".into()), day: Day::Today };
        let fetch = |_: &str| Fetched::Data(pune());
        let new = handle_ask(&ask, fetch).unwrap();
        assert!(new.contains("cloudy right now") && !new.contains("as of"), "{new}");
        // Asked again a moment later: still new.
        assert_eq!(handle_ask(&ask, |_: &str| Fetched::Down).unwrap(), new);
        // Fetched ten minutes ago.
        let url = url_for(ask.place.as_deref());
        let ten_minutes_ago = Instant::now().checked_sub(Duration::from_secs(10 * 60)).expect("the clock is older than ten minutes");
        memory().answers.insert(url.clone(), (ten_minutes_ago, pune()));
        let old = handle_ask(&ask, |_: &str| Fetched::Down).unwrap();
        assert!(old.starts_with("In Pune, India it is 24 degrees Celsius and cloudy, as of 10 minutes ago.") && !old.contains("right now"), "{old}");
        // The service is down and the answer is older than the cache keeps it: said as the last check, with the time.
        let two_hours_ago = Instant::now().checked_sub(Duration::from_secs(2 * 3600 + 30)).expect("the clock is older than two hours");
        memory().answers.insert(url, (two_hours_ago, pune()));
        memory().failed_at = Some(Instant::now());
        let stale = handle_ask(&ask, |_: &str| Fetched::Down).unwrap();
        assert!(stale.starts_with("When I last checked, in Pune, India it is 24 degrees Celsius and cloudy, as of 2 hours ago.") && !stale.contains("right now"), "{stale}");
        // The words, and a forecast that never said "right now".
        assert_eq!((ago(Duration::from_secs(60)), ago(Duration::from_secs(5 * 60 + 59)), ago(Duration::from_secs(3600)), ago(Duration::from_secs(3 * 3600 + 1))), ("1 minute ago".to_string(), "5 minutes ago".to_string(), "1 hour ago".to_string(), "3 hours ago".to_string()));
        assert_eq!(as_of("Tomorrow in Pune it will be warm.", Duration::from_secs(900)), "Tomorrow in Pune it will be warm.");
        assert_eq!(as_of("It is warm right now.", Duration::from_secs(119)), "It is warm right now.");
        forget_everything();
    }

    #[test]
    fn data_that_is_not_what_was_expected_makes_no_answer() {
        let ask = Ask { place: Some("pune".into()), day: Day::Today };
        assert_eq!(compose(&ask, &json!({})), None);
        assert_eq!(compose(&ask, &json!({ "current_condition": [] })), None);
        assert_eq!(compose(&ask, &json!({ "current_condition": [{ "temp_C": "warm" }] })), None);
        // Only the temperature is needed: the rest is left out when it is missing.
        let thin = json!({ "current_condition": [{ "temp_C": "19" }], "nearest_area": [] });
        assert_eq!(compose(&ask, &thin).unwrap(), "In Pune it is 19 degrees Celsius right now.");
    }

    #[test]
    fn a_fetched_answer_is_kept_for_a_while_and_a_failure_rests_the_service() {
        let _only = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        forget_everything();
        let calls = AtomicUsize::new(0);
        let ask = Ask { place: Some("pune".into()), day: Day::Today };
        let fetch = |_: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            Fetched::Data(pune())
        };
        let first = handle_ask(&ask, fetch).unwrap();
        let second = handle_ask(&ask, fetch).unwrap();
        assert_eq!((first, calls.load(Ordering::SeqCst)), (second, 1), "the second question did not ask the service again");
        // Tomorrow is read from the same data.
        assert!(handle_ask(&Ask { place: Some("pune".into()), day: Day::Tomorrow }, fetch).unwrap().starts_with("Tomorrow in Pune"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Another place is another request; a failure is not an answer and rests the service.
        forget_everything();
        let broken = |_: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            Fetched::Down
        };
        let missed = handle_ask(&ask, broken).unwrap();
        assert!(missed.contains("won't search the web"));
        let after_failure = calls.load(Ordering::SeqCst);
        assert_eq!(handle_ask(&ask, broken).as_deref(), Some(missed.as_str()));
        assert!(handle_ask(&Ask { place: Some("delhi".into()), day: Day::Today }, broken).unwrap().contains("won't search the web"));
        assert_eq!(calls.load(Ordering::SeqCst), after_failure, "while it rests nothing is asked: no waiting for the same time-out again");
        forget_everything();
    }

    /// Found in a review of the first version: a place the service does not know (a mis-heard name, a 404) used to rest the service for a
    /// minute, so every weather question after it went to the slow agent.
    #[test]
    fn an_unknown_place_is_not_an_outage_and_does_not_hold_back_the_next_question() {
        let _only = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        forget_everything();
        let calls = AtomicUsize::new(0);
        let service = |url: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            if url.contains("nowhere") { Fetched::Unknown } else { Fetched::Data(pune()) }
        };
        assert_eq!(handle_ask(&Ask { place: Some("nowhere".into()), day: Day::Today }, service), None, "the agent takes it");
        let said = handle_ask(&Ask { place: Some("pune".into()), day: Day::Today }, service);
        assert!(said.is_some_and(|s| s.starts_with("In Pune")), "the next question is answered at once");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        forget_everything();
    }

    #[test]
    fn a_tool_result_is_data_an_unknown_place_or_an_outage() {
        let body = format!("URL: https://wttr.in/Pune?format=j2\n\n{}", pune());
        assert!(matches!(classify(true, Some(200), &body), Fetched::Data(_)));
        assert!(matches!(classify(true, Some(200), "URL: x\n\nUnknown location; please try ~51.5,-0.1"), Fetched::Unknown), "200 with something else: not an outage");
        assert!(matches!(classify(true, Some(404), "HTTP 404 Not Found\nURL: x\n\nUnknown location"), Fetched::Unknown));
        assert!(matches!(classify(true, Some(400), ""), Fetched::Unknown));
        // What the real service sent for a place that does not exist (HTTP 500 with these words), as the tool returned it.
        let real = "<untrusted:99008cd9b1cc>\nHTTP 500 Internal Server Error\nURL: https://wttr.in/Xqzvbtl?format=j2\n\nlocation not found: upstream error: opencage: invalid response\n\n";
        assert!(matches!(classify(true, Some(500), real), Fetched::Unknown), "a place that is not found is not an outage, whatever the status");
        assert!(matches!(classify(true, Some(500), "<untrusted:x>\nHTTP 500 Internal Server Error\n\nupstream connect error"), Fetched::Down), "an error of the service itself is");
        for outage in [classify(false, None, "ERROR [TIMEOUT]"), classify(true, Some(500), ""), classify(true, Some(503), ""), classify(true, Some(429), ""), classify(true, Some(408), ""), classify(true, Some(0), ""), classify(true, None, "")] {
            assert!(matches!(outage, Fetched::Down));
        }
    }

    /// The compact format has no hours: the conditions of today are the current ones, and no chance of rain or description of tomorrow is
    /// said that the service did not give.
    #[test]
    fn the_compact_format_gives_a_shorter_answer_without_inventing_what_it_lacks() {
        let mut compact = pune();
        for day in compact["weather"].as_array_mut().unwrap() {
            day.as_object_mut().unwrap().remove("hourly");
        }
        assert_eq!(
            compose(&Ask { place: Some("pune".into()), day: Day::Today }, &compact).unwrap(),
            "In Pune, India it is 24 degrees Celsius and cloudy right now. Humidity is 61 percent, with hardly any wind. Today's high is 32 degrees and the low is 22."
        );
        assert_eq!(compose(&Ask { place: Some("pune".into()), day: Day::Tomorrow }, &compact).unwrap(), "Tomorrow in Pune, India it will be between 22 and 31 degrees Celsius.");
    }

    #[test]
    fn the_json_is_found_in_the_text_the_tool_returns_however_it_is_wrapped() {
        let body = pune().to_string();
        for wrapped in [
            format!("URL: https://wttr.in/Pune?format=j2

{body}"),
            format!("[untrusted content abc123]
URL: https://wttr.in/Pune?format=j2

{body}
[end of untrusted content abc123]"),
            format!("Title: 
URL: https://wttr.in/Pune?format=j2

{body}
… [page cut]"),
        ] {
            assert_eq!(json_in(&wrapped).unwrap()["current_condition"][0]["temp_C"], "24", "{wrapped}");
        }
        assert_eq!(json_in("URL: https://wttr.in/Nowhere

Unknown location; please try ~51.5,-0.1"), None);
        assert_eq!(json_in("URL: https://wttr.in/Pune

{\"weather\": []}"), None, "no current conditions: not an answer");
        assert_eq!(json_in("{broken"), None);
    }

    /// The whole path with the real tool host, as the API caller and with no one to ask for approval: the service is on the default allow
    /// list, so the call is made without a question, and its answer reads as a spoken answer (needs the internet).
    #[test]
    #[ignore = "needs the internet"]
    fn the_real_tool_host_fetches_the_weather_without_asking() {
        use aqua_mcp_adapters::browser::BrowserBackend;
        use aqua_mcp_adapters::{new_browser_client, new_windows_client};
        let _only = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        forget_everything();
        let root = std::env::temp_dir().join(format!("aqua_weather_host_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data").join("tools")).unwrap();
        let host = crate::tool_host::build_tool_host(&root.join("data"), std::sync::Arc::new(BrowserBackend::new(&root.join("browser"), new_browser_client())), std::sync::Arc::new(Mutex::new(new_windows_client())));
        for (place, day) in [("pune", Day::Today), ("new delhi", Day::Today), ("bikaner", Day::Tomorrow)] {
            let started = Instant::now();
            let said = handle_ask(&Ask { place: Some(place.into()), day }, |url| fetch_through_host(&host, url));
            eprintln!("MEASURE {place} {day:?}: {} ms | {said:?}", started.elapsed().as_millis());
            if said.is_none() {
                use aqua_tool_host::{Caller, RequestContext, ToolRequest};
                let raw = host.call_blocking(ToolRequest { tool: "web.fetch".into(), arguments: json!({ "url": "https://wttr.in/pune?format=j2" }), context: RequestContext::new(Caller::Api) });
                panic!("{place}: no answer; the tool said ok={} error={:?} text={:?}", raw.ok, raw.error, raw.text.chars().take(300).collect::<String>());
            }
        }
        drop(host);
        forget_everything();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Against the real service (needs the internet): the live data has the shape the answer reads. Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "needs the internet"]
    fn the_real_service_answers_in_the_expected_shape() {
        let _only = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        forget_everything();
        for place in ["Pune", "New Delhi", "Bikaner", "Paris", "Springfield"] {
            let started = Instant::now();
            let out = std::process::Command::new("curl").args(["-s", "-m", "12", "-H", "Accept: application/json", &url_for(Some(place))]).output().unwrap();
            let data: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{place}: not JSON: {e}"));
            let said = compose(&Ask { place: Some(place.to_lowercase()), day: Day::Today }, &data).unwrap_or_else(|| panic!("{place}: no answer from {data}"));
            let tomorrow = compose(&Ask { place: Some(place.to_lowercase()), day: Day::Tomorrow }, &data).unwrap();
            eprintln!("MEASURE {place}: {} ms | {said} | {tomorrow}", started.elapsed().as_millis());
        }
    }
}
