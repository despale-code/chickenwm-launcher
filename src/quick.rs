//! Quick actions for the launcher: clipboard history (`c `), unit and currency conversion
//! (`10 km to mi`, `100 usd in eur`), emoji search (`:smile`), `kill <process>`, recent files
//! (`r `), web search shortcuts (`g `, `yt `, `gh `, `ddg `, `wiki `), and frecency ranking for
//! apps (how often and how recently each one was launched).

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Web search shortcuts
// ---------------------------------------------------------------------------------------------

pub const WEB_SHORTCUTS: [(&str, &str, &str); 6] = [
    ("g", "Google", "https://www.google.com/search?q="),
    ("yt", "YouTube", "https://www.youtube.com/results?search_query="),
    ("gh", "GitHub", "https://github.com/search?q="),
    ("ddg", "DuckDuckGo", "https://duckduckgo.com/?q="),
    ("wiki", "Wikipedia", "https://en.wikipedia.org/w/index.php?search="),
    ("maps", "Google Maps", "https://www.google.com/maps/search/"),
];

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `g rust lifetimes` → ("Google", url).
pub fn web_search(query: &str) -> Option<(&'static str, String)> {
    let (prefix, rest) = query.split_once(' ')?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    WEB_SHORTCUTS
        .iter()
        .find(|(p, _, _)| p.eq_ignore_ascii_case(prefix))
        .map(|(_, name, base)| (*name, format!("{base}{}", url_encode(rest))))
}

// ---------------------------------------------------------------------------------------------
// Unit conversion
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dim {
    Length,
    Mass,
    Volume,
    Data,
    Time,
    Speed,
    Area,
    Temperature,
}

/// (names, dimension, size in the dimension's base unit). Temperatures are handled apart.
const UNITS: &[(&[&str], Dim, f64)] = &[
    (&["mm", "millimeter", "millimeters", "millimetre", "millimetres"], Dim::Length, 0.001),
    (&["cm", "centimeter", "centimeters", "centimetre", "centimetres"], Dim::Length, 0.01),
    (&["m", "meter", "meters", "metre", "metres"], Dim::Length, 1.0),
    (&["km", "kilometer", "kilometers", "kilometre", "kilometres"], Dim::Length, 1000.0),
    (&["in", "inch", "inches", "\""], Dim::Length, 0.0254),
    (&["ft", "foot", "feet", "'"], Dim::Length, 0.3048),
    (&["yd", "yard", "yards"], Dim::Length, 0.9144),
    (&["mi", "mile", "miles"], Dim::Length, 1609.344),
    (&["nmi", "nauticalmile", "nauticalmiles"], Dim::Length, 1852.0),
    (&["mg", "milligram", "milligrams"], Dim::Mass, 0.001),
    (&["g", "gram", "grams"], Dim::Mass, 1.0),
    (&["kg", "kilogram", "kilograms", "kilo", "kilos"], Dim::Mass, 1000.0),
    (&["t", "tonne", "tonnes", "ton", "tons"], Dim::Mass, 1_000_000.0),
    (&["oz", "ounce", "ounces"], Dim::Mass, 28.349_523_125),
    (&["lb", "lbs", "pound", "pounds"], Dim::Mass, 453.592_37),
    (&["st", "stone", "stones"], Dim::Mass, 6350.293_18),
    (&["ml", "milliliter", "milliliters", "millilitre", "millilitres"], Dim::Volume, 0.001),
    (&["l", "liter", "liters", "litre", "litres"], Dim::Volume, 1.0),
    (&["gal", "gallon", "gallons"], Dim::Volume, 3.785_411_784),
    (&["qt", "quart", "quarts"], Dim::Volume, 0.946_352_946),
    (&["pt", "pint", "pints"], Dim::Volume, 0.473_176_473),
    (&["cup", "cups"], Dim::Volume, 0.24),
    (&["floz", "fluidounce", "fluidounces"], Dim::Volume, 0.029_573_529_6),
    (&["b", "byte", "bytes"], Dim::Data, 1.0),
    (&["kb", "kilobyte", "kilobytes"], Dim::Data, 1e3),
    (&["mb", "megabyte", "megabytes"], Dim::Data, 1e6),
    (&["gb", "gigabyte", "gigabytes"], Dim::Data, 1e9),
    (&["tb", "terabyte", "terabytes"], Dim::Data, 1e12),
    (&["kib", "kibibyte", "kibibytes"], Dim::Data, 1024.0),
    (&["mib", "mebibyte", "mebibytes"], Dim::Data, 1_048_576.0),
    (&["gib", "gibibyte", "gibibytes"], Dim::Data, 1_073_741_824.0),
    (&["tib", "tebibyte", "tebibytes"], Dim::Data, 1_099_511_627_776.0),
    (&["ms", "millisecond", "milliseconds"], Dim::Time, 0.001),
    (&["s", "sec", "secs", "second", "seconds"], Dim::Time, 1.0),
    (&["min", "mins", "minute", "minutes"], Dim::Time, 60.0),
    (&["h", "hr", "hrs", "hour", "hours"], Dim::Time, 3600.0),
    (&["d", "day", "days"], Dim::Time, 86400.0),
    (&["wk", "week", "weeks"], Dim::Time, 604_800.0),
    (&["mps", "m/s"], Dim::Speed, 1.0),
    (&["kmh", "km/h", "kph"], Dim::Speed, 1.0 / 3.6),
    (&["mph"], Dim::Speed, 0.447_04),
    (&["kn", "knot", "knots"], Dim::Speed, 0.514_444),
    (&["m2", "sqm"], Dim::Area, 1.0),
    (&["km2", "sqkm"], Dim::Area, 1e6),
    (&["ft2", "sqft"], Dim::Area, 0.092_903_04),
    (&["ha", "hectare", "hectares"], Dim::Area, 10_000.0),
    (&["acre", "acres"], Dim::Area, 4046.856_422_4),
    (&["c", "°c", "celsius"], Dim::Temperature, 0.0),
    (&["f", "°f", "fahrenheit"], Dim::Temperature, 1.0),
    (&["k", "kelvin"], Dim::Temperature, 2.0),
];

fn unit(name: &str) -> Option<(Dim, f64, &'static str)> {
    let n = name.trim().to_lowercase();
    UNITS.iter().find(|(names, _, _)| names.contains(&n.as_str())).map(|(names, d, f)| (*d, *f, names[0]))
}

/// Splits `10 km to mi` / `10km in miles` / `3.5 gb -> mib` into (amount, from, to).
fn split_conversion(q: &str) -> Option<(f64, String, String)> {
    let q = q.trim().to_lowercase();
    let (left, right) = [" to ", " in ", " -> ", " = "].iter().find_map(|sep| q.split_once(sep))?;
    let left = left.trim();
    let num_end = left
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.' || *c == ',' || *c == '-' || *c == '+'))
        .map_or(left.len(), |(i, _)| i);
    // "2,5" (one comma, no dot, not followed by exactly three digits) is a decimal comma;
    // otherwise commas group thousands ("1,000").
    let raw = &left[..num_end];
    let decimal_comma = !raw.contains('.')
        && raw.matches(',').count() == 1
        && raw.split(',').nth(1).is_some_and(|f| f.len() != 3);
    let raw = if decimal_comma { raw.replace(',', ".") } else { raw.replace(',', "") };
    let amount: f64 = raw.parse().ok()?;
    let from = left[num_end..].trim().replace(' ', "");
    let to = right.trim().replace(' ', "");
    (!from.is_empty() && !to.is_empty()).then_some((amount, from, to))
}

fn fmt_number(v: f64) -> String {
    if v != 0.0 && (v.abs() >= 1e12 || v.abs() < 1e-4) {
        return format!("{v:.4e}");
    }
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// `10 km to mi` → ("6.213712 mi", "10 km = 6.213712 mi").
pub fn convert_units(q: &str) -> Option<(String, String)> {
    let (amount, from, to) = split_conversion(q)?;
    let (fd, ff, fname) = unit(&from)?;
    let (td, tf, tname) = unit(&to)?;
    if fd != td {
        return None;
    }
    let value = if fd == Dim::Temperature {
        let celsius = match ff as u8 {
            0 => amount,
            1 => (amount - 32.0) * 5.0 / 9.0,
            _ => amount - 273.15,
        };
        match tf as u8 {
            0 => celsius,
            1 => celsius * 9.0 / 5.0 + 32.0,
            _ => celsius + 273.15,
        }
    } else {
        amount * ff / tf
    };
    let result = format!("{} {tname}", fmt_number(value));
    Some((result.clone(), format!("{} {fname} = {result}", fmt_number(amount))))
}

// ---------------------------------------------------------------------------------------------
// Currency
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Rates {
    /// Units of each currency per 1 EUR.
    pub per_eur: HashMap<String, f64>,
    pub fetched: u64,
    /// Date the rates are for (from the source).
    pub date: String,
}

pub type SharedRates = Arc<RwLock<Option<Rates>>>;

fn rates_path() -> PathBuf {
    chickenwm_core::config::config_dir().join("currency-rates.json")
}

/// Offline fallback rates (units per 1 EUR) used before the first network fetch succeeds.
fn fallback_rates() -> Rates {
    let per_eur = [
        ("EUR".to_owned(), 1.0),
        ("USD".to_owned(), 1.09),
        ("GBP".to_owned(), 0.84),
        ("JPY".to_owned(), 162.5),
        ("CHF".to_owned(), 0.94),
        ("CAD".to_owned(), 1.50),
        ("AUD".to_owned(), 1.64),
        ("CNY".to_owned(), 7.78),
        ("INR".to_owned(), 91.5),
        ("TRY".to_owned(), 37.5),
        ("BRL".to_owned(), 6.10),
        ("KRW".to_owned(), 1485.0),
        ("SEK".to_owned(), 11.40),
        ("NOK".to_owned(), 11.80),
        ("PLN".to_owned(), 4.30),
    ]
    .into_iter()
    .collect();
    Rates {
        per_eur,
        fetched: 0,
        date: "offline-fallback".to_owned(),
    }
}

/// Human-readable staleness indicator for cached currency rates.
pub fn rates_staleness_label(rates: &Rates, current_ts: u64) -> String {
    if rates.fetched == 0 {
        return format!("ECB {} \u{f444} offline fallback", rates.date);
    }
    let age_secs = current_ts.saturating_sub(rates.fetched);
    if age_secs < 3600 {
        format!("ECB {} \u{f444} updated {}m ago", rates.date, (age_secs / 60).max(1))
    } else if age_secs < 86_400 {
        format!("ECB {} \u{f444} updated {}h ago", rates.date, age_secs / 3600)
    } else {
        let days = age_secs / 86_400;
        format!("ECB {} \u{f444} stale ({days}d old, offline)", rates.date)
    }
}

/// Loads cached ECB rates (falling back to built-in offline rates) and refreshes them in the
/// background when older than 12 hours.
pub fn spawn_rates() -> SharedRates {
    let initial = std::fs::read_to_string(rates_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Rates>(&t).ok())
        .unwrap_or_else(fallback_rates);
    let shared: SharedRates = Arc::new(RwLock::new(Some(initial)));
    let out = Arc::clone(&shared);
    std::thread::spawn(move || {
        let stale = out
            .read()
            .as_ref()
            .is_none_or(|r| r.fetched == 0 || now().saturating_sub(r.fetched) > 12 * 3600);
        if !stale {
            return;
        }
        use std::os::windows::process::CommandExt;
        let Ok(o) = std::process::Command::new("curl.exe")
            .args([
                "-sSL",
                "--fail",
                "--max-time",
                "15",
                "https://api.frankfurter.app/latest?from=EUR",
            ])
            .creation_flags(0x0800_0000)
            .output()
        else {
            return;
        };
        #[derive(Deserialize)]
        struct Resp {
            date: String,
            rates: HashMap<String, f64>,
        }
        let Ok(resp) = serde_json::from_slice::<Resp>(&o.stdout) else {
            return;
        };
        let mut per_eur = resp.rates;
        per_eur.insert("EUR".into(), 1.0);
        let rates = Rates {
            per_eur,
            fetched: now(),
            date: resp.date,
        };
        if let Ok(s) = serde_json::to_string(&rates) {
            let _ = std::fs::write(rates_path(), s);
        }
        *out.write() = Some(rates);
    });
    shared
}

const CURRENCY_ALIASES: [(&str, &str); 12] = [
    ("$", "USD"),
    ("dollar", "USD"),
    ("dollars", "USD"),
    ("€", "EUR"),
    ("euro", "EUR"),
    ("euros", "EUR"),
    ("£", "GBP"),
    ("pound sterling", "GBP"),
    ("₺", "TRY"),
    ("lira", "TRY"),
    ("¥", "JPY"),
    ("yen", "JPY"),
];

/// `100 usd to eur` → ("92.31 EUR", "100 USD = 92.31 EUR (ECB 2026-10-17 · updated 2h ago)").
pub fn convert_currency(q: &str, rates: &Rates) -> Option<(String, String)> {
    let (amount, from, to) = split_conversion(q)?;
    let code = |s: &str| -> Option<String> {
        let s = s.trim().to_lowercase();
        let c = CURRENCY_ALIASES
            .iter()
            .find(|(a, _)| *a == s)
            .map(|(_, c)| c.to_string())
            .unwrap_or_else(|| s.to_uppercase());
        rates.per_eur.contains_key(&c).then_some(c)
    };
    let (f, t) = (code(&from)?, code(&to)?);
    let value = amount / rates.per_eur[&f] * rates.per_eur[&t];
    let result = format!("{value:.2} {t}");
    let staleness = rates_staleness_label(rates, now());
    Some((
        result.clone(),
        format!("{} {f} = {result} ({staleness})", fmt_number(amount)),
    ))
}

// ---------------------------------------------------------------------------------------------
// Emoji
// ---------------------------------------------------------------------------------------------

/// (emoji, keywords).
const EMOJI: &[(&str, &str)] = &[
    ("😀", "grinning smile happy"), ("😃", "smiley happy joy"), ("😄", "smile happy laugh"),
    ("😁", "grin beaming"), ("😆", "laughing satisfied"), ("😅", "sweat smile relief"),
    ("😂", "joy tears laugh lol"), ("🤣", "rofl rolling laugh"), ("🙂", "slight smile"),
    ("🙃", "upside down"), ("😉", "wink"), ("😊", "blush smile"), ("😇", "innocent halo angel"),
    ("🥰", "love hearts smiling"), ("😍", "heart eyes love"), ("🤩", "star struck wow"),
    ("😘", "kiss blow"), ("😋", "yum delicious tongue"), ("😛", "tongue"), ("😜", "wink tongue crazy"),
    ("🤪", "zany crazy"), ("🤑", "money mouth rich"), ("🤗", "hug hugging"), ("🤭", "hand over mouth oops"),
    ("🤫", "shush quiet"), ("🤔", "thinking hmm"), ("🤐", "zipper mouth secret"), ("🤨", "raised eyebrow suspicious"),
    ("😐", "neutral meh"), ("😑", "expressionless"), ("😶", "no mouth speechless"), ("😏", "smirk"),
    ("😒", "unamused"), ("🙄", "eye roll"), ("😬", "grimace awkward"), ("😌", "relieved calm"),
    ("😔", "pensive sad"), ("😪", "sleepy"), ("😴", "sleeping zzz"), ("😷", "mask sick"),
    ("🤒", "thermometer ill fever"), ("🤢", "nauseated sick"), ("🤮", "vomit puke"), ("🥵", "hot sweating"),
    ("🥶", "cold freezing"), ("😵", "dizzy"), ("🤯", "exploding head mind blown"), ("🤠", "cowboy"),
    ("🥳", "party celebrate"), ("😎", "cool sunglasses"), ("🤓", "nerd glasses"), ("🧐", "monocle"),
    ("😕", "confused"), ("😟", "worried"), ("🙁", "frown slightly sad"), ("😮", "open mouth wow surprised"),
    ("😲", "astonished shocked"), ("😳", "flushed embarrassed"), ("🥺", "pleading puppy eyes"), ("😦", "frowning"),
    ("😨", "fearful scared"), ("😰", "anxious sweat"), ("😢", "cry sad tear"), ("😭", "sob crying loud"),
    ("😱", "scream fear"), ("😖", "confounded"), ("😣", "persevere"), ("😞", "disappointed"),
    ("😓", "downcast sweat"), ("😩", "weary tired"), ("😫", "tired"), ("🥱", "yawn bored"),
    ("😤", "triumph huff"), ("😡", "pouting rage angry"), ("😠", "angry mad"), ("🤬", "cursing swearing"),
    ("😈", "smiling devil"), ("💀", "skull dead"), ("💩", "poop"), ("🤡", "clown"), ("👻", "ghost"),
    ("👽", "alien"), ("🤖", "robot"), ("😺", "cat smile"), ("❤️", "heart love red"), ("🧡", "orange heart"),
    ("💛", "yellow heart"), ("💚", "green heart"), ("💙", "blue heart"), ("💜", "purple heart"),
    ("🖤", "black heart"), ("🤍", "white heart"), ("💔", "broken heart"), ("💯", "hundred 100 perfect"),
    ("💥", "boom collision"), ("💫", "dizzy star"), ("💦", "sweat drops"), ("💤", "zzz sleep"),
    ("👋", "wave hello bye"), ("🤚", "raised back hand"), ("✋", "raised hand stop high five"), ("👌", "ok hand"),
    ("🤌", "pinched fingers"), ("✌️", "victory peace"), ("🤞", "crossed fingers luck"), ("🤟", "love you gesture"),
    ("🤘", "rock on horns"), ("🤙", "call me shaka"), ("👈", "point left"), ("👉", "point right"),
    ("👆", "point up"), ("👇", "point down"), ("☝️", "index up"), ("👍", "thumbs up like yes +1"),
    ("👎", "thumbs down dislike no -1"), ("✊", "fist raised"), ("👊", "punch fist bump"), ("👏", "clap applause"),
    ("🙌", "raising hands hooray"), ("👐", "open hands"), ("🙏", "pray please thanks"), ("✍️", "writing"),
    ("💪", "muscle strong flex"), ("🧠", "brain smart"), ("👀", "eyes look"), ("👁️", "eye"),
    ("🫡", "salute"), ("🤝", "handshake deal"), ("🐔", "chicken"), ("🐓", "rooster"), ("🐣", "hatching chick"),
    ("🐤", "baby chick"), ("🥚", "egg"), ("🍳", "cooking fried egg"), ("🐶", "dog puppy"), ("🐱", "cat kitten"),
    ("🦊", "fox"), ("🐻", "bear"), ("🐼", "panda"), ("🐸", "frog"), ("🐵", "monkey"), ("🦄", "unicorn"),
    ("🐝", "bee"), ("🦋", "butterfly"), ("🐢", "turtle slow"), ("🐍", "snake python"), ("🦀", "crab rust"),
    ("🐙", "octopus"), ("🐳", "whale docker"), ("🌱", "seedling plant"), ("🌲", "tree evergreen"),
    ("🌸", "cherry blossom flower"), ("🌹", "rose"), ("🌻", "sunflower"), ("🍀", "four leaf clover luck"),
    ("🍁", "maple leaf autumn"), ("🍂", "fallen leaves autumn fall"), ("🍎", "apple red"), ("🍌", "banana"),
    ("🍓", "strawberry"), ("🍕", "pizza"), ("🍔", "burger hamburger"), ("🍟", "fries"), ("🌮", "taco"),
    ("🍣", "sushi"), ("🍜", "ramen noodles"), ("🍪", "cookie"), ("🎂", "birthday cake"), ("🍰", "cake"),
    ("☕", "coffee hot tea"), ("🍵", "tea"), ("🍺", "beer"), ("🍷", "wine"), ("🥂", "cheers toast"),
    ("🌍", "earth globe world"), ("🌙", "moon night"), ("⭐", "star"), ("🌟", "glowing star"),
    ("☀️", "sun sunny"), ("⛅", "cloudy partly"), ("🌧️", "rain"), ("⛈️", "storm thunder"), ("❄️", "snow"),
    ("🔥", "fire hot lit"), ("💧", "droplet water"), ("🌈", "rainbow"), ("⚡", "zap lightning fast"),
    ("🎉", "tada party celebrate"), ("🎊", "confetti"), ("🎁", "gift present"), ("🎈", "balloon"),
    ("🏆", "trophy win"), ("🥇", "gold medal first"), ("⚽", "soccer football"), ("🏀", "basketball"),
    ("🎮", "video game controller"), ("🎲", "dice game"), ("🎵", "music note"), ("🎶", "music notes"),
    ("🎧", "headphones"), ("🎸", "guitar"), ("📷", "camera photo"), ("🎬", "clapper movie film"),
    ("📺", "tv television"), ("💻", "laptop computer"), ("🖥️", "desktop computer"), ("⌨️", "keyboard"),
    ("🖱️", "mouse computer"), ("📱", "phone mobile"), ("🔋", "battery"), ("🔌", "plug electric"),
    ("💡", "bulb idea"), ("🔦", "flashlight"), ("📚", "books"), ("📖", "book open"), ("📝", "memo note"),
    ("✏️", "pencil"), ("📎", "paperclip"), ("📌", "pin pushpin"), ("📅", "calendar date"), ("📈", "chart up"),
    ("📉", "chart down"), ("📊", "bar chart"), ("📁", "folder"), ("🗑️", "trash wastebasket"), ("🔒", "lock locked"),
    ("🔓", "unlock"), ("🔑", "key"), ("🔨", "hammer"), ("🛠️", "tools"), ("⚙️", "gear settings"),
    ("🧪", "test tube"), ("🔬", "microscope"), ("💊", "pill"), ("💰", "money bag"), ("💸", "money flying"),
    ("💳", "credit card"), ("✉️", "envelope mail"), ("📧", "email"), ("📦", "package box"), ("🚀", "rocket launch ship"),
    ("✈️", "airplane"), ("🚗", "car"), ("🚲", "bicycle bike"), ("🏠", "house home"), ("🏢", "office building"),
    ("⏰", "alarm clock"), ("⌛", "hourglass"), ("⏳", "hourglass flowing"), ("✅", "check mark done yes"),
    ("☑️", "check box"), ("✔️", "check"), ("❌", "cross x no"), ("❎", "cross mark button"), ("⚠️", "warning"),
    ("⛔", "no entry"), ("🚫", "prohibited forbidden"), ("❓", "question"), ("❗", "exclamation"),
    ("➕", "plus"), ("➖", "minus"), ("➡️", "right arrow"), ("⬅️", "left arrow"), ("⬆️", "up arrow"),
    ("⬇️", "down arrow"), ("🔄", "arrows refresh"), ("🆗", "ok button"), ("🆕", "new"), ("🆒", "cool button"),
    ("🇹🇷", "turkey flag tr"), ("🇺🇸", "usa flag us america"), ("🇬🇧", "uk flag gb britain"), ("🇩🇪", "germany flag de"),
    ("🏁", "checkered flag finish"), ("🚩", "red flag"), ("🏳️‍🌈", "rainbow flag pride"),
];

/// Emoji whose keywords contain every word typed after `:`.
pub fn emoji_search(q: &str, limit: usize) -> Vec<(&'static str, &'static str)> {
    let words: Vec<String> = q.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return EMOJI.iter().take(limit).copied().collect();
    }
    let mut hits: Vec<(usize, (&str, &str))> = EMOJI
        .iter()
        .filter(|(_, kw)| words.iter().all(|w| kw.split(' ').any(|k| k.starts_with(w.as_str()))))
        // Exact first-keyword matches first.
        .map(|e| (usize::from(!e.1.split(' ').next().is_some_and(|k| k == words[0])), *e))
        .collect();
    hits.sort_by_key(|h| h.0);
    hits.into_iter().take(limit).map(|h| h.1).collect()
}

// ---------------------------------------------------------------------------------------------
// Processes (kill)
// ---------------------------------------------------------------------------------------------

/// Running processes whose exe name contains `q`, grouped by name: (name, pids).
pub fn find_processes(q: &str) -> Vec<(String, Vec<u32>)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    let q = q.trim().to_lowercase();
    let own = std::process::id();
    let mut groups: Vec<(String, Vec<u32>)> = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return groups };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]);
            if e.th32ProcessID > 4 && e.th32ProcessID != own && name.to_lowercase().contains(&q) {
                match groups.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                    Some(g) => g.1.push(e.th32ProcessID),
                    None => groups.push((name, vec![e.th32ProcessID])),
                }
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    // Exact name first, then shorter names.
    groups.sort_by_key(|(n, _)| (!n.to_lowercase().trim_end_matches(".exe").eq(&q), n.len()));
    groups
}

/// Terminates the processes; returns how many were ended.
pub fn kill(pids: &[u32]) -> usize {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    let mut n = 0;
    for &pid in pids {
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                if TerminateProcess(h, 1).is_ok() {
                    n += 1;
                }
                let _ = CloseHandle(h);
            }
        }
    }
    n
}

// ---------------------------------------------------------------------------------------------
// Recent files
// ---------------------------------------------------------------------------------------------

/// Shortcuts in the user's Recent folder, newest first: (display name, .lnk path).
pub fn recent_files(q: &str, limit: usize) -> Vec<(String, PathBuf)> {
    let Some(dir) = dirs::data_dir().map(|d| d.join(r"Microsoft\Windows\Recent")) else { return Vec::new() };
    let q = q.trim().to_lowercase();
    let mut files: Vec<(SystemTime, String, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().is_none_or(|x| !x.eq_ignore_ascii_case("lnk")) {
                return None;
            }
            let name = path.file_stem()?.to_string_lossy().into_owned();
            if !q.is_empty() && !name.to_lowercase().contains(&q) {
                return None;
            }
            Some((e.metadata().and_then(|m| m.modified()).ok()?, name, path))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().take(limit).map(|(_, n, p)| (n, p)).collect()
}

// ---------------------------------------------------------------------------------------------
// Clipboard history (memory only, with pinned items, password-manager exclusion, & image cap)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipEntry {
    pub text: String,
    pub pinned: bool,
}

pub type ClipHistory = Arc<RwLock<Vec<ClipEntry>>>;

const CLIP_MAX: usize = 50;
/// Maximum clipboard image payload (4 MiB) before ignoring heavy bitmap copies.
pub const MAX_CLIP_IMAGE_BYTES: usize = 4 * 1024 * 1024;

/// Toggles the pinned state of `text` in `history`, keeping pinned items at the top.
pub fn toggle_pin_clipboard(history: &ClipHistory, text: &str) -> bool {
    let mut h = history.write();
    let mut now_pinned = false;
    if let Some(entry) = h.iter_mut().find(|e| e.text == text) {
        entry.pinned = !entry.pinned;
        now_pinned = entry.pinned;
    }
    h.sort_by_key(|e| !e.pinned);
    now_pinned
}

/// Watches the clipboard for text and keeps the last 50 distinct entries, newest first (pinned
/// items stay at the top). Kept in memory only (never written to disk), skips content that
/// password managers mark as private (`ExcludeClipboardContentFromMonitorProcessing`,
/// `CanIncludeInClipboardHistory`, `ClipboardViewerIgnore`), and caps image size.
pub fn spawn_clipboard_watch() -> ClipHistory {
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    const CF_DIB: u32 = 8;
    let history: ClipHistory = Arc::default();
    let out = Arc::clone(&history);
    std::thread::spawn(move || unsafe {
        let private_exclude =
            RegisterClipboardFormatW(windows::core::w!("ExcludeClipboardContentFromMonitorProcessing"));
        let private_history =
            RegisterClipboardFormatW(windows::core::w!("CanIncludeInClipboardHistory"));
        let viewer_ignore =
            RegisterClipboardFormatW(windows::core::w!("ClipboardViewerIgnore"));
        let mut last_seq = GetClipboardSequenceNumber();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let seq = GetClipboardSequenceNumber();
            if seq == last_seq {
                continue;
            }
            last_seq = seq;
            if (private_exclude != 0 && IsClipboardFormatAvailable(private_exclude).is_ok())
                || (viewer_ignore != 0 && IsClipboardFormatAvailable(viewer_ignore).is_ok())
            {
                continue;
            }
            if OpenClipboard(None).is_err() {
                continue;
            }
            // Check CanIncludeInClipboardHistory == 0 (password managers set DWORD 0)
            let mut blocked_by_history_flag = false;
            if private_history != 0 && IsClipboardFormatAvailable(private_history).is_ok() {
                if let Ok(h) = GetClipboardData(private_history) {
                    let hg = windows::Win32::Foundation::HGLOBAL(h.0);
                    let p = GlobalLock(hg) as *const u32;
                    if !p.is_null() {
                        blocked_by_history_flag = *p == 0;
                        let _ = GlobalUnlock(hg);
                    }
                }
            }
            // Enforce image size cap when CF_DIB is present
            let mut oversized_image = false;
            if IsClipboardFormatAvailable(CF_DIB).is_ok() {
                if let Ok(h) = GetClipboardData(CF_DIB) {
                    let hg = windows::Win32::Foundation::HGLOBAL(h.0);
                    if GlobalSize(hg) > MAX_CLIP_IMAGE_BYTES {
                        oversized_image = true;
                    }
                }
            }
            let mut text = None;
            if !blocked_by_history_flag
                && !oversized_image
                && IsClipboardFormatAvailable(CF_UNICODETEXT).is_ok()
            {
                if let Ok(h) = GetClipboardData(CF_UNICODETEXT) {
                    let hg = windows::Win32::Foundation::HGLOBAL(h.0);
                    let p = GlobalLock(hg) as *const u16;
                    if !p.is_null() {
                        let mut len = 0;
                        while *p.add(len) != 0 && len < 100_000 {
                            len += 1;
                        }
                        text = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)));
                        let _ = GlobalUnlock(hg);
                    }
                }
            }
            let _ = CloseClipboard();
            if let Some(t) = text.filter(|t| !t.trim().is_empty() && t.len() <= 20_000) {
                let mut h = out.write();
                let was_pinned = h.iter().find(|x| x.text == t).is_some_and(|x| x.pinned);
                h.retain(|x| x.text != t);
                let insert_pos = if was_pinned {
                    0
                } else {
                    h.iter().take_while(|x| x.pinned).count()
                };
                h.insert(
                    insert_pos,
                    ClipEntry {
                        text: t,
                        pinned: was_pinned,
                    },
                );
                h.truncate(CLIP_MAX);
            }
        }
    });
    history
}

/// One line for the result list: whitespace collapsed, cut at `max` characters.
pub fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        flat.chars().take(max - 1).collect::<String>() + "..."
    } else {
        flat
    }
}

// ---------------------------------------------------------------------------------------------
// Prefix Handlers (`w `, `c `, `kill `, `r `, and `?` cheat-sheet)
// ---------------------------------------------------------------------------------------------

use crate::{LauncherEntry, LauncherItemKind};
use chickenwm_core::ipc::{
    send_ipc_command_blocking, ClientInfo, IpcCommand, IpcResponse, PIPE_NAME_WM,
};

/// Prefix cheat-sheet entries shown when the user types `?` (or as a quick reference).
pub const PREFIX_CHEAT_SHEET: [(&str, &str, &str); 8] = [
    ("w ", "w <window>", "Switch to an open window across workspaces 1..10"),
    ("c ", "c <text>", "Search clipboard history (Ctrl+P pins/unpins item)"),
    ("r ", "r <file>", "Open recent files from Windows Recent items"),
    ("kill ", "kill <process>", "Terminate running processes by name"),
    ("= ", "= <math>", "Evaluate math expressions (e.g. = 144 * 1.5)"),
    ("> ", "> <command>", "Run a PowerShell command in Windows Terminal"),
    (":", ":<emoji>", "Search emoji by keyword (e.g. :thumbs up)"),
    ("g ", "g / yt / gh / ddg / wiki", "Search the web or convert units/currency (10 km to mi, 100 usd to eur)"),
];

/// Builds cheat-sheet `LauncherEntry` rows when the query is `?` or `? <filter>`.
pub fn handle_cheat_sheet_prefix(q: &str) -> Option<Vec<LauncherEntry>> {
    let filter = q
        .strip_prefix("? ")
        .or_else(|| (q == "?").then_some(""))?
        .trim()
        .to_ascii_lowercase();
    let mut out = Vec::with_capacity(PREFIX_CHEAT_SHEET.len());
    for (prefix, syntax, desc) in PREFIX_CHEAT_SHEET {
        if filter.is_empty()
            || syntax.to_ascii_lowercase().contains(&filter)
            || desc.to_ascii_lowercase().contains(&filter)
        {
            out.push(LauncherEntry {
                title: format!("{syntax}  \u{f444}  {desc}"),
                subtitle: format!("Prefix `{prefix}` \u{f444} Press Enter to copy prefix"),
                badge: "HELP",
                kind: LauncherItemKind::CopyText {
                    text: prefix.to_string(),
                    kind_label: "Prefix",
                },
                score: 100,
            });
        }
    }
    Some(out)
}

/// Handler for `w <query>`: queries `chickenwm-wm` for open windows and filters them.
pub fn handle_window_prefix(q: &str) -> Option<Vec<LauncherEntry>> {
    let win_query = q.strip_prefix("w ")?;
    let wq = win_query.trim().to_ascii_lowercase();
    let clients: Vec<ClientInfo> =
        match send_ipc_command_blocking(PIPE_NAME_WM, &IpcCommand::QueryClients) {
            Ok(IpcResponse::Clients(c)) => c,
            _ => Vec::new(),
        };
    let mut out = Vec::new();
    for c in clients {
        if wq.is_empty()
            || c.title.to_ascii_lowercase().contains(&wq)
            || c.process_name.to_ascii_lowercase().contains(&wq)
        {
            out.push(LauncherEntry {
                title: c.title,
                subtitle: format!(
                    "Workspace {} \u{f444} {} (0x{:X})",
                    c.workspace_id, c.process_name, c.hwnd
                ),
                badge: "WIN",
                kind: LauncherItemKind::WindowSwitch {
                    hwnd: c.hwnd,
                    workspace_id: c.workspace_id,
                },
                score: 100,
            });
        }
    }
    Some(out)
}

/// Handler for `c <query>`: searches in-memory clipboard history, showing pinned items first.
pub fn handle_clipboard_prefix(
    q: &str,
    clip: &ClipHistory,
    max_results: usize,
) -> Option<Vec<LauncherEntry>> {
    let cq = q.strip_prefix("c ")?.trim().to_lowercase();
    let mut out = Vec::new();
    for entry in clip
        .read()
        .iter()
        .filter(|e| cq.is_empty() || e.text.to_lowercase().contains(&cq))
    {
        let pin_tag = if entry.pinned { "\u{f0403} Pinned \u{f444} " } else { "" };
        out.push(LauncherEntry {
            title: one_line(&entry.text, 80),
            subtitle: format!(
                "{pin_tag}{} characters \u{f444} Enter to copy \u{f444} Ctrl+P to {}",
                entry.text.chars().count(),
                if entry.pinned { "unpin" } else { "pin" }
            ),
            badge: if entry.pinned { "PIN" } else { "CLIP" },
            kind: LauncherItemKind::CopyText {
                text: entry.text.clone(),
                kind_label: if entry.pinned { "Pinned Clip" } else { "Clipboard" },
            },
            score: u32::from(entry.pinned) * 100,
        });
    }
    out.truncate(max_results * 2);
    Some(out)
}

/// Handler for `kill <process>`: finds matching processes and builds termination entries.
pub fn handle_kill_prefix(q: &str, max_results: usize) -> Option<Vec<LauncherEntry>> {
    let kq = q.strip_prefix("kill ")?;
    let mut out = Vec::new();
    if !kq.trim().is_empty() {
        for (name, pids) in find_processes(kq).into_iter().take(max_results) {
            out.push(LauncherEntry {
                title: format!("Kill {name}"),
                subtitle: format!(
                    "{} process{} (pid {})",
                    pids.len(),
                    if pids.len() == 1 { "" } else { "es" },
                    pids.iter()
                        .take(6)
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                badge: "KILL",
                kind: LauncherItemKind::Kill { pids },
                score: 0,
            });
        }
    }
    Some(out)
}

/// Handler for `r <query>`: lists matching shortcuts from the Windows Recent folder.
pub fn handle_recent_prefix(q: &str, max_results: usize) -> Option<Vec<LauncherEntry>> {
    let rq = q.strip_prefix("r ")?;
    let mut out = Vec::new();
    for (name, path) in recent_files(rq, max_results) {
        out.push(LauncherEntry {
            title: name,
            subtitle: "Recent file".to_owned(),
            badge: "FILE",
            kind: LauncherItemKind::Open {
                target: path.to_string_lossy().into_owned(),
                kind_label: "Recent",
            },
            score: 0,
        });
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Frecency (smooth exponential half-life decay + "forget this entry" action, shared via chickenwm-core)
// ---------------------------------------------------------------------------------------------

pub use chickenwm_core::scoring::FrecencyStore as Frecency;
#[allow(unused_imports)]
pub use chickenwm_core::scoring::UseRecord as Use;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        assert_eq!(convert_units("10 km to mi").unwrap().0, "6.213712 mi");
        assert_eq!(convert_units("100 f in c").unwrap().0, "37.777778 c");
        assert_eq!(convert_units("0 c to k").unwrap().0, "273.15 k");
        assert_eq!(convert_units("1 gib -> mb").unwrap().0, "1073.741824 mb");
        assert_eq!(convert_units("2,5 kg to lb").unwrap().0, "5.511557 lb");
        assert_eq!(convert_units("1,000 m to km").unwrap().0, "1 km");
        assert_eq!(convert_units("25kg to lb").unwrap().0, "55.115566 lb");
        assert_eq!(convert_units("1 h to kg"), None);
        assert_eq!(convert_units("firefox"), None);
    }

    #[test]
    fn currency() {
        let rates = Rates {
            per_eur: [
                ("EUR".to_owned(), 1.0),
                ("USD".to_owned(), 1.1),
                ("TRY".to_owned(), 40.0),
            ]
            .into(),
            fetched: 0,
            date: "2026-10-17".into(),
        };
        let (res, detail) = convert_currency("10 eur to usd", &rates).unwrap();
        assert_eq!(res, "11.00 USD");
        assert!(detail.contains("offline fallback"));
        assert_eq!(
            convert_currency("110 $ in lira", &rates).unwrap().0,
            "4000.00 TRY"
        );
        assert_eq!(convert_currency("10 km to mi", &rates), None);
    }

    #[test]
    fn web_and_emoji() {
        let (name, url) = web_search("yt rust & c++").unwrap();
        assert_eq!(name, "YouTube");
        assert!(url.ends_with("rust+%26+c%2B%2B"));
        assert_eq!(web_search("g "), None);
        assert_eq!(web_search("firefox"), None);
        assert_eq!(emoji_search("thumbs up", 3)[0].0, "👍");
        assert_eq!(emoji_search("chick", 5).len(), 3);
    }

    #[test]
    fn frecency_weights_and_forget() {
        let t = 10_000_000;
        assert_eq!(Frecency::weigh(None, t), 0);
        let fresh = Frecency::weigh(Some(Use { count: 3, last: t }), t);
        let week_old = Frecency::weigh(
            Some(Use {
                count: 3,
                last: t - 7 * 86_400,
            }),
            t,
        );
        let month_old = Frecency::weigh(
            Some(Use {
                count: 3,
                last: t - 40 * 86_400,
            }),
            t,
        );
        assert!(fresh > week_old && week_old > month_old && month_old > 0);

        let mut frec = Frecency::load_from(PathBuf::new());
        frec.insert_raw("app.exe", 5, now());
        assert!(frec.score("app.exe") > 0);
        assert!(frec.forget("app.exe"));
        assert_eq!(frec.score("app.exe"), 0);
    }

    #[test]
    fn clipboard_pin_and_cheat_sheet() {
        let clip: ClipHistory = Arc::new(RwLock::new(vec![
            ClipEntry {
                text: "first".into(),
                pinned: false,
            },
            ClipEntry {
                text: "second".into(),
                pinned: false,
            },
        ]));
        assert!(toggle_pin_clipboard(&clip, "second"));
        let rows = handle_clipboard_prefix("c ", &clip, 10).unwrap();
        assert_eq!(rows[0].title, "second");
        assert_eq!(rows[0].badge, "PIN");

        let help = handle_cheat_sheet_prefix("?").unwrap();
        assert_eq!(help.len(), PREFIX_CHEAT_SHEET.len());
    }
}
