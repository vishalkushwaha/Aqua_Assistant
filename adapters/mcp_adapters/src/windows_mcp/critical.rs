//! Sign-in and payment steps: the places where desktop control stops and asks (card R6-34, MagenticLite's "critical points").
//!
//! An agent that is told to "book the ticket" reaches a page that wants a password, a one-time code or a card number, and the next click pays or signs in as the person. Whatever
//! the settings are, Aqua asks first there: the question is not removed by full access, and it is never put to the voice (a spoken request cannot answer it, and a television can
//! say the wake word). The question never repeats what was typed.
//!
//! **A heuristic, and a last line of defence, not the control.** It reads the title of the window in front and the text of the last snapshot (the same text the model reads), by
//! phrases: a card number, an expiry date or CVV, "place your order" and "pay now"; a password asked for with a way to sign in; a one-time code; a title that says sign in,
//! checkout or payment. It does not read a page it was not shown, a screenshot (no text), a page in a language other than English, or an amount in a field with no label. A page
//! that talks about passwords and has a "Sign in" link is taken for a sign-in step, and costs one question. The sets in the tests were written by the one who wrote the phrases,
//! so they show what the phrases do on pages of the kinds that were thought of, and nothing about real pages.

use super::guard::{is_drag, Caller, DenyKind, Foreground, Verdict};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    SignIn,
    Payment,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::SignIn => "sign-in",
            Kind::Payment => "payment",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Critical {
    pub kind: Kind,
    /// What was seen, for the question ("the window is called \"Checkout\"").
    pub why: String,
}

/// Words in a title that make a step of it: signing in, or paying.
const TITLE_SIGN_IN: &[&str] = &[
    "sign in", "signin", "log in", "login", "sign on", "verify your identity", "two step verification", "2 step verification", "two factor", "2fa", "multi factor", "mfa", "one time password",
    "one time code", "otp", "verification code", "passkey", "enter your credentials", "windows security", "enter your password", "enter password",
];
const TITLE_PAYMENT: &[&str] = &[
    "checkout", "check out", "secure payment", "payment", "pay now", "billing", "place order", "order review", "review your order", "complete your purchase", "upi", "net banking", "netbanking",
    "send money", "bank transfer", "pay bill", "pay your bill",
];
/// A title with one of these is reading material about the subject: a title alone does not make a step of it, and a password mentioned beside a "Sign in" link (the header of
/// most documentation) does not either. The words of a card or a payment are not forgiven for it.
const TITLE_READING: &[&str] = &["docs", "documentation", "api reference", "guide", "tutorial", "blog", "wikipedia", "how to", "stack overflow"];

/// Phrases in the text of a window that mean money is about to move.
const TEXT_PAYMENT: &[&str] = &[
    "card number", "credit card number", "debit card number", "cvv", "cvc", "cvc2", "expiration date", "expiry date", "name on card", "pay now", "place order", "place your order", "complete purchase",
    "complete your purchase", "confirm payment", "confirm order", "confirm purchase", "make payment", "make a payment", "proceed to pay", "submit payment", "pay securely", "upi pin", "enter upi pin",
    "confirm transfer", "transfer funds", "send money",
];
/// A secret being asked for ...
const TEXT_SECRET: &[&str] = &[
    "enter your password", "enter password", "type your password", "password", "passcode", "one time password", "one time code", "verification code", "security code", "authenticator app",
    "two step verification", "2 step verification",
];
/// ... and a way of signing in on the same screen.
const TEXT_SIGN_IN: &[&str] = &["sign in", "log in", "login", "forgot password", "forgot your password", "keep me signed in", "stay signed in", "remember me", "use another account", "show password"];

/// The text as words in lower case, with one space between and one at each end, so that a phrase is found only as whole words ("cvv" is not in "cvvx"; "sign-in" is "sign in").
fn words_padded(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push(' ');
    let mut last_space = true;
    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    if !last_space {
        out.push(' ');
    }
    out
}

fn found<'a>(padded: &str, phrases: &'a [&'a str]) -> Option<&'a str> {
    phrases.iter().copied().find(|p| padded.contains(&format!(" {p} ")))
}

/// Is this window a sign-in or payment step? `title` is the window's, `text` is what the last snapshot said (may be empty).
pub fn critical_point(title: &str, text: &str) -> Option<Critical> {
    let t = words_padded(title);
    let x = words_padded(text);
    // The text decides first: it says what the screen asks for.
    if let Some(p) = found(&x, TEXT_PAYMENT) {
        return Some(Critical { kind: Kind::Payment, why: format!("the screen says \"{p}\"") });
    }
    let reading = found(&t, TITLE_READING).is_some();
    if let (false, Some(secret), Some(way)) = (reading, found(&x, TEXT_SECRET), found(&x, TEXT_SIGN_IN)) {
        return Some(Critical { kind: Kind::SignIn, why: format!("the screen asks for a \"{secret}\" and offers \"{way}\"") });
    }
    // A title alone is read as a step unless it is reading material about the subject. Signing in is looked for first: "Netbanking Login" is a sign-in.
    if reading {
        return None;
    }
    if let Some(p) = found(&t, TITLE_SIGN_IN) {
        return Some(Critical { kind: Kind::SignIn, why: format!("the window is called \"{}\" ({p})", title.trim()) });
    }
    if let Some(p) = found(&t, TITLE_PAYMENT) {
        return Some(Critical { kind: Kind::Payment, why: format!("the window is called \"{}\" ({p})", title.trim()) });
    }
    None
}

/// What the last snapshot of a window showed, kept for a little while.
#[derive(Debug, Clone)]
pub struct View {
    pub exe: String,
    pub window: isize,
    pub at_ms: u64,
    pub critical: Option<Critical>,
}

/// A snapshot is read as the state of the screen for this long; after it, only the title says.
pub const VIEW_FRESH_MS: u64 = 180_000;

impl View {
    pub fn of(fg: &Foreground, snapshot_text: &str, now_ms: u64) -> Self {
        Self { exe: fg.exe.clone(), window: fg.window, at_ms: now_ms, critical: critical_point(&fg.title, snapshot_text) }
    }
}

/// Is the window in front at a critical point: by its title, or by what the last snapshot of the same window showed a short while ago?
pub fn critical_for(fg: &Foreground, view: Option<&View>, now_ms: u64) -> Option<Critical> {
    critical_point(&fg.title, "").or_else(|| {
        view.filter(|v| now_ms.saturating_sub(v.at_ms) <= VIEW_FRESH_MS && v.exe.eq_ignore_ascii_case(&fg.exe) && (v.window == 0 || fg.window == 0 || v.window == fg.window))
            .and_then(|v| v.critical.clone())
    })
}

/// What a call does to the screen, in words, for a question.
fn action_of(tool: &str, args: &Value) -> Option<String> {
    if tool.eq_ignore_ascii_case("Click") {
        return Some("click".into());
    }
    if tool.eq_ignore_ascii_case("Type") || tool.eq_ignore_ascii_case("MultiEdit") {
        return Some("type".into());
    }
    if tool.eq_ignore_ascii_case("MultiSelect") {
        return Some("select".into());
    }
    if tool.eq_ignore_ascii_case("Shortcut") {
        let combo = args.get("shortcut").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        return Some(format!("press {combo}"));
    }
    (tool.eq_ignore_ascii_case("Move") && is_drag("Move", args)).then(|| "drag".to_string())
}

/// The verdict at a critical point: a call that acts is asked about, whatever the settings, and refused for the voice; a refusal stays one, and a call that only looks is left alone.
/// Nothing that was typed is in the question.
pub fn critical_verdict(base: Verdict, tool: &str, args: &Value, caller: Caller, fg: Option<&Foreground>, critical: Option<&Critical>) -> Verdict {
    let (Some(critical), Some(action)) = (critical, action_of(tool, args)) else { return base };
    if matches!(base, Verdict::Deny(..)) {
        return base;
    }
    let window = fg.map(|f| if f.title.is_empty() { f.exe.clone() } else { format!("{} (\"{}\")", f.exe, f.title) }).unwrap_or_else(|| "the window in front".into());
    if caller == Caller::Voice {
        return Verdict::Deny(
            DenyKind::Blocked,
            format!("{action} by voice is refused: {window} looks like a {} step ({}), and Aqua asks about those in the app, never by voice.", critical.kind.name(), critical.why),
        );
    }
    Verdict::Ask(format!("{action} at what looks like a {} step ({}) in {window}. Aqua asks here whatever the settings.", critical.kind.name(), critical.why))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fg(exe: &str, title: &str) -> Foreground {
        Foreground { exe: exe.into(), title: title.into(), window: 0 }
    }

    /// Pages that are a sign-in or payment step, as a title and the words a snapshot of them has.
    const STEPS: &[(&str, &str, Kind)] = &[
        ("Sign in to your account", "", Kind::SignIn),
        ("Welcome", "Email or phone Password Log in Remember me", Kind::SignIn),
        ("Google Accounts", "Enter your password Next Forgot password?", Kind::SignIn),
        ("GitHub", "Username or email address Password Sign in Forgot password?", Kind::SignIn),
        ("Account", "Show password Keep me signed in Sign in", Kind::SignIn),
        ("IRCTC", "User name Password Captcha Login", Kind::SignIn),
        ("2-step verification", "", Kind::SignIn),
        ("Verify", "Enter the verification code we sent to your phone. Use another account", Kind::SignIn),
        ("Windows Security", "Enter your credentials", Kind::SignIn),
        ("Checkout - Shop", "", Kind::Payment),
        ("Payment", "", Kind::Payment),
        ("Netbanking Login", "", Kind::SignIn),
        ("Review your order", "", Kind::Payment),
        ("Amazon.in", "Card number Name on card Expiry date CVV Pay now", Kind::Payment),
        ("PayPal", "Confirm payment", Kind::Payment),
        ("Shop", "Order summary Place your order", Kind::Payment),
        ("Zomato", "Proceed to pay", Kind::Payment),
        ("Flipkart", "Complete purchase", Kind::Payment),
        ("Razorpay", "Enter UPI PIN", Kind::Payment),
        ("My bank", "Transfer funds to a new payee. Confirm transfer", Kind::Payment),
        ("Pay your bill", "", Kind::Payment),
        ("Secure Payment - Edge", "Card number CVV", Kind::Payment),
    ];

    /// Pages that are neither: a title and the words a snapshot of them has.
    const ORDINARY: &[(&str, &str)] = &[
        ("Inbox (3) - Gmail", "Compose Inbox Starred Sent Drafts Search mail"),
        ("Untitled - Notepad", ""),
        ("Home - BBC News", "Sign in Register Home News Sport Weather Search"),
        ("Python docs", "hashlib password hashing scrypt Sign in"),
        ("hashlib - Python 3 documentation", "password hashing"),
        ("Stripe Docs - Payment Intents", "Create a PaymentIntent on your server"),
        ("Payment API reference", ""),
        ("How to choose a strong password - Security Blog", "Use a long passphrase. Continue reading"),
        ("Amazon.in: Laptops", "Add to cart Buy now Price 49,999 In stock Free delivery"),
        ("Calculator", "Seven Eight Nine Divide Equals"),
        ("Visual Studio Code", "main.rs Explorer Search Source Control Run and Debug"),
        ("Downloads", "invoice.pdf notes.txt photo.png"),
        ("Spotify", "Liked Songs Recently played Search"),
        ("Weather in Pune", "32 degrees Partly cloudy Humidity 54 percent"),
        ("Wikipedia - Credit card", "A credit card is a payment card issued to users"),
        ("YouTube", "Home Shorts Subscriptions Sign in"),
        ("Settings - Display", "Resolution Scale Night light Brightness"),
        ("Task list", "Write the report Call the bank Order cartridges"),
        ("Chess.com", "Play Puzzles Learn Watch Log in Sign up"),
        ("Photos", "Recent Albums People Places"),
        ("Cricbuzz", "Live scores Schedule Series News"),
        ("Aqua Assistant", "Press and hold to speak"),
    ];

    #[test]
    fn a_sign_in_or_payment_step_is_found_by_its_title_or_by_what_the_screen_says() {
        let mut missed = Vec::new();
        for (title, text, kind) in STEPS {
            match critical_point(title, text) {
                Some(c) if c.kind == *kind => {}
                other => missed.push(format!("{title:?} / {text:?}: {other:?}")),
            }
        }
        assert!(missed.is_empty(), "{} of {} steps were missed or named wrongly:\n{}", missed.len(), STEPS.len(), missed.join("\n"));
    }

    #[test]
    fn an_ordinary_page_is_not_taken_for_one_and_a_title_about_the_subject_is_reading_material() {
        let mut flagged = Vec::new();
        for (title, text) in ORDINARY {
            if let Some(c) = critical_point(title, text) {
                flagged.push(format!("{title:?} / {text:?}: {c:?}"));
            }
        }
        assert!(flagged.is_empty(), "{} of {} ordinary pages were taken for a step:\n{}", flagged.len(), ORDINARY.len(), flagged.join("\n"));
        // What it does take wrongly, and costs one question: a page about passwords that also has a sign-in link.
        assert!(critical_point("Account security", "Change your password. Sign in with a passkey").is_some());
        // Phrases are whole words: not inside another word.
        assert_eq!(critical_point("Shop", "cvvx pay nowhere placeorder"), None);
    }

    #[test]
    fn the_phrases_are_read_as_whole_words_in_any_case_and_punctuation() {
        assert!(critical_point("x", "PLEASE ENTER YOUR CARD NUMBER").is_some());
        assert!(critical_point("x", "Card-number: ____").is_some(), "a hyphen is a space: card-number is card number");
        assert!(critical_point("Sign-in", "").is_some(), "sign-in is sign in");
        assert!(critical_point("x", "Log in with your Password").is_some());
        assert!(critical_point("x", "Password").is_none(), "a secret asked for with no way of signing in is not enough");
        assert!(critical_point("x", "Sign in").is_none(), "a way of signing in with no secret asked for is not enough");
    }

    #[test]
    fn a_snapshot_is_read_for_a_while_for_the_window_it_showed_and_the_title_alone_always() {
        let page = Foreground { exe: "msedge.exe".into(), title: "Welcome".into(), window: 77 };
        let view = View::of(&page, "Password Sign in Remember me", 1_000);
        assert!(view.critical.is_some());
        assert!(critical_for(&page, Some(&view), 1_000 + 60_000).is_some(), "shortly after, the same window");
        assert!(critical_for(&page, Some(&view), 1_000 + VIEW_FRESH_MS + 1).is_none(), "long after, only the title says");
        assert!(critical_for(&Foreground { window: 78, ..page.clone() }, Some(&view), 2_000).is_none(), "another window of the same program");
        assert!(critical_for(&Foreground { exe: "notepad.exe".into(), ..page.clone() }, Some(&view), 2_000).is_none(), "another program");
        assert!(critical_for(&Foreground { window: 0, ..page.clone() }, Some(&view), 2_000).is_some(), "a handle that is not known cannot say it is another window");
        assert!(critical_for(&fg("chrome.exe", "Checkout - Shop"), None, 0).is_some(), "the title, with no snapshot at all");
        assert!(critical_for(&fg("chrome.exe", "Home"), None, 0).is_none());
    }

    #[test]
    fn at_a_critical_point_a_call_that_acts_is_asked_about_whatever_the_settings_and_never_by_voice() {
        let step = critical_point("Checkout", "Card number CVV Pay now");
        let window = fg("msedge.exe", "Checkout - Shop");
        let secret = "hunter2-secret-card-4111111111111111";
        for (tool, args, action) in [
            ("Click", json!({"loc": [1, 2]}), "click"),
            ("Type", json!({"text": secret}), "type"),
            ("MultiEdit", json!({"text": secret}), "type"),
            ("MultiSelect", json!({"loc": [1, 2]}), "select"),
            ("Shortcut", json!({"shortcut": "enter"}), "press enter"),
            ("Move", json!({"loc": [1, 2], "drag": true}), "drag"),
        ] {
            let v = critical_verdict(Verdict::Allow, tool, &args, Caller::Agent, Some(&window), step.as_ref());
            match &v {
                Verdict::Ask(q) => assert!(q.starts_with(action) && q.contains("payment step") && !q.contains(secret) && !q.contains("4111"), "{tool}: {q}"),
                other => panic!("{tool}: {other:?}"),
            }
            assert!(matches!(critical_verdict(Verdict::Allow, tool, &args, Caller::Api, Some(&window), step.as_ref()), Verdict::Ask(_)), "{tool}: the API is asked too");
            assert!(matches!(critical_verdict(Verdict::Allow, tool, &args, Caller::Voice, Some(&window), step.as_ref()), Verdict::Deny(DenyKind::Blocked, why) if why.contains("never by voice")), "{tool}: the voice is refused");
            assert!(matches!(critical_verdict(Verdict::Deny(DenyKind::Blocked, "no".into()), tool, &args, Caller::Agent, Some(&window), step.as_ref()), Verdict::Deny(DenyKind::Blocked, why) if why == "no"), "{tool}: a refusal stays the guard's");
            assert!(matches!(critical_verdict(Verdict::Ask("the guard asks".into()), tool, &args, Caller::Agent, Some(&window), step.as_ref()), Verdict::Ask(q) if q.contains("step")), "{tool}: one question, the critical one");
            assert_eq!(critical_verdict(Verdict::Allow, tool, &args, Caller::Agent, Some(&window), None), Verdict::Allow, "{tool}: not at a critical point");
        }
        // A call that only looks is not asked about, and a window with no snapshot and no title that says so is the guard's as before.
        for tool in ["Snapshot", "Screenshot", "Scroll", "Wait", "App"] {
            assert_eq!(critical_verdict(Verdict::Allow, tool, &json!({}), Caller::Agent, Some(&window), step.as_ref()), Verdict::Allow, "{tool}");
        }
        assert_eq!(critical_verdict(Verdict::Allow, "Move", &json!({"loc": [1, 2]}), Caller::Agent, Some(&window), step.as_ref()), Verdict::Allow, "a pointer move only looks");
        let sign_in = critical_point("Sign in", "");
        assert!(matches!(critical_verdict(Verdict::Allow, "Click", &json!({"loc": [1, 2]}), Caller::Agent, None, sign_in.as_ref()), Verdict::Ask(q) if q.contains("sign-in step") && q.contains("the window in front")));
    }
}
