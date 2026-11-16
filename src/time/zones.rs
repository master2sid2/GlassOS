//! Time zones: the standard time offset and the daylight saving rule.
//!
//! This is not the whole tz database but the current rules of major cities: the history
//! of transitions is not needed while the clock shows today's date.

/// What the transition time is counted from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Base {
    /// Universal time.
    Utc,
    /// The zone's standard time.
    Standard,
    /// The wall clock: before the switch to summer time it is standard time, before the switch back — summer time.
    Wall,
}

/// The day and hour of a transition.
#[derive(Clone, Copy)]
pub struct Edge {
    pub month: u8,
    /// Which occurrence of the weekday in the month, -1 — the last.
    pub nth: i8,
    /// The weekday, 0 — Sunday.
    pub weekday: u8,
    /// An offset from the found day in days (in Israel, the Friday before Sunday).
    pub shift: i8,
    /// Minutes from the start of the day.
    pub at: i16,
    pub base: Base,
}

/// Summer time: when it starts and ends, and by how many minutes it shifts.
pub struct Rule {
    pub start: Edge,
    pub end: Edge,
    pub save: i32,
}

const SUN: u8 = 0;
const THU: u8 = 4;
const FRI: u8 = 5;
const LAST: i8 = -1;

const fn edge(month: u8, nth: i8, weekday: u8, at: i16, base: Base) -> Edge {
    Edge { month, nth, weekday, shift: 0, at, base }
}

/// The European Union and neighbours: the last Sunday of March and October, 01:00 UTC.
pub static EU: Rule = Rule { start: edge(3, LAST, SUN, 60, Base::Utc), end: edge(10, LAST, SUN, 60, Base::Utc), save: 60 };
/// The USA and Canada: the second Sunday of March and the first of November, 02:00 on the clock.
pub static US: Rule = Rule { start: edge(3, 2, SUN, 120, Base::Wall), end: edge(11, 1, SUN, 120, Base::Wall), save: 60 };
/// South-eastern Australia: the first Sunday of October and the first of April.
pub static AU: Rule = Rule { start: edge(10, 1, SUN, 120, Base::Standard), end: edge(4, 1, SUN, 120, Base::Standard), save: 60 };
/// New Zealand: the last Sunday of September and the first of April.
pub static NZ: Rule = Rule { start: edge(9, LAST, SUN, 120, Base::Standard), end: edge(4, 1, SUN, 120, Base::Standard), save: 60 };
/// Chile: the first Sunday of September and the first of April, at Saturday midnight.
pub static CHILE: Rule = Rule { start: edge(9, 1, SUN, 240, Base::Utc), end: edge(4, 1, SUN, 180, Base::Utc), save: 60 };
/// Israel: the Friday before the last Sunday of March, the last Sunday of October.
pub static ISRAEL: Rule = Rule {
    start: Edge { month: 3, nth: LAST, weekday: SUN, shift: -2, at: 120, base: Base::Standard },
    end: edge(10, LAST, SUN, 120, Base::Wall),
    save: 60,
};
/// Egypt: the last Friday of April and the last Thursday of October at midnight.
pub static EGYPT: Rule = Rule { start: edge(4, LAST, FRI, 0, Base::Standard), end: edge(10, LAST, THU, 24 * 60, Base::Wall), save: 60 };
/// Moldova: as in the European Union, only by its own clock.
pub static MOLDOVA: Rule = Rule { start: edge(3, LAST, SUN, 120, Base::Wall), end: edge(10, LAST, SUN, 180, Base::Wall), save: 60 };
/// Lebanon: the last Sundays of March and October at midnight.
pub static LEBANON: Rule = Rule { start: edge(3, LAST, SUN, 0, Base::Wall), end: edge(10, LAST, SUN, 0, Base::Wall), save: 60 };
/// Newfoundland and Halifax follow the US rule.
const CANADA: &Rule = &US;

/// A city: a tz-database-style name, the standard offset in minutes and the rule.
pub struct City {
    pub name: &'static str,
    pub offset: i32,
    pub rule: Option<&'static Rule>,
}

const fn city(name: &'static str, offset: i32, rule: Option<&'static Rule>) -> City {
    City { name, offset, rule }
}

const H: i32 = 60;

pub static CITIES: &[City] = &[
    city("Africa/Abidjan", 0, None),
    city("Africa/Cairo", 2 * H, Some(&EGYPT)),
    city("Africa/Casablanca", H, None),
    city("Africa/Johannesburg", 2 * H, None),
    city("Africa/Lagos", H, None),
    city("Africa/Nairobi", 3 * H, None),
    city("America/Anchorage", -9 * H, Some(&US)),
    city("America/Bogota", -5 * H, None),
    city("America/Buenos_Aires", -3 * H, None),
    city("America/Caracas", -4 * H, None),
    city("America/Chicago", -6 * H, Some(&US)),
    city("America/Denver", -7 * H, Some(&US)),
    city("America/Halifax", -4 * H, Some(CANADA)),
    city("America/Lima", -5 * H, None),
    city("America/Los_Angeles", -8 * H, Some(&US)),
    city("America/Mexico_City", -6 * H, None),
    city("America/Montevideo", -3 * H, None),
    city("America/New_York", -5 * H, Some(&US)),
    city("America/Phoenix", -7 * H, None),
    city("America/Santiago", -4 * H, Some(&CHILE)),
    city("America/Sao_Paulo", -3 * H, None),
    city("America/St_Johns", -3 * H - 30, Some(CANADA)),
    city("America/Toronto", -5 * H, Some(&US)),
    city("America/Vancouver", -8 * H, Some(&US)),
    city("Asia/Almaty", 5 * H, None),
    city("Asia/Baghdad", 3 * H, None),
    city("Asia/Baku", 4 * H, None),
    city("Asia/Bangkok", 7 * H, None),
    city("Asia/Beirut", 2 * H, Some(&LEBANON)),
    city("Asia/Dhaka", 6 * H, None),
    city("Asia/Dubai", 4 * H, None),
    city("Asia/Ho_Chi_Minh", 7 * H, None),
    city("Asia/Hong_Kong", 8 * H, None),
    city("Asia/Jakarta", 7 * H, None),
    city("Asia/Jerusalem", 2 * H, Some(&ISRAEL)),
    city("Asia/Kabul", 4 * H + 30, None),
    city("Asia/Karachi", 5 * H, None),
    city("Asia/Kathmandu", 5 * H + 45, None),
    city("Asia/Kolkata", 5 * H + 30, None),
    city("Asia/Manila", 8 * H, None),
    city("Asia/Novosibirsk", 7 * H, None),
    city("Asia/Riyadh", 3 * H, None),
    city("Asia/Seoul", 9 * H, None),
    city("Asia/Shanghai", 8 * H, None),
    city("Asia/Singapore", 8 * H, None),
    city("Asia/Taipei", 8 * H, None),
    city("Asia/Tashkent", 5 * H, None),
    city("Asia/Tbilisi", 4 * H, None),
    city("Asia/Tehran", 3 * H + 30, None),
    city("Asia/Tokyo", 9 * H, None),
    city("Asia/Vladivostok", 10 * H, None),
    city("Asia/Yangon", 6 * H + 30, None),
    city("Asia/Yekaterinburg", 5 * H, None),
    city("Asia/Yerevan", 4 * H, None),
    city("Atlantic/Azores", -H, Some(&EU)),
    city("Atlantic/Cape_Verde", -H, None),
    city("Atlantic/Reykjavik", 0, None),
    city("Australia/Adelaide", 9 * H + 30, Some(&AU)),
    city("Australia/Brisbane", 10 * H, None),
    city("Australia/Darwin", 9 * H + 30, None),
    city("Australia/Hobart", 10 * H, Some(&AU)),
    city("Australia/Melbourne", 10 * H, Some(&AU)),
    city("Australia/Perth", 8 * H, None),
    city("Australia/Sydney", 10 * H, Some(&AU)),
    city("Europe/Amsterdam", H, Some(&EU)),
    city("Europe/Athens", 2 * H, Some(&EU)),
    city("Europe/Belgrade", H, Some(&EU)),
    city("Europe/Berlin", H, Some(&EU)),
    city("Europe/Brussels", H, Some(&EU)),
    city("Europe/Bucharest", 2 * H, Some(&EU)),
    city("Europe/Budapest", H, Some(&EU)),
    city("Europe/Chisinau", 2 * H, Some(&MOLDOVA)),
    city("Europe/Copenhagen", H, Some(&EU)),
    city("Europe/Dublin", 0, Some(&EU)),
    city("Europe/Helsinki", 2 * H, Some(&EU)),
    city("Europe/Istanbul", 3 * H, None),
    city("Europe/Kyiv", 2 * H, Some(&EU)),
    city("Europe/Lisbon", 0, Some(&EU)),
    city("Europe/London", 0, Some(&EU)),
    city("Europe/Madrid", H, Some(&EU)),
    city("Europe/Minsk", 3 * H, None),
    city("Europe/Moscow", 3 * H, None),
    city("Europe/Oslo", H, Some(&EU)),
    city("Europe/Paris", H, Some(&EU)),
    city("Europe/Prague", H, Some(&EU)),
    city("Europe/Riga", 2 * H, Some(&EU)),
    city("Europe/Rome", H, Some(&EU)),
    city("Europe/Samara", 4 * H, None),
    city("Europe/Sofia", 2 * H, Some(&EU)),
    city("Europe/Stockholm", H, Some(&EU)),
    city("Europe/Tallinn", 2 * H, Some(&EU)),
    city("Europe/Vienna", H, Some(&EU)),
    city("Europe/Vilnius", 2 * H, Some(&EU)),
    city("Europe/Warsaw", H, Some(&EU)),
    city("Europe/Zurich", H, Some(&EU)),
    city("Pacific/Auckland", 12 * H, Some(&NZ)),
    city("Pacific/Fiji", 12 * H, None),
    city("Pacific/Honolulu", -10 * H, None),
];

/// Offsets without cities: for those whose city is not in the list.
pub const FIXED: [i32; 27] = [
    -12 * H, -11 * H, -10 * H, -9 * H, -8 * H, -7 * H, -6 * H, -5 * H, -4 * H, -3 * H, -2 * H, -H, 0,
    H, 2 * H, 3 * H, 4 * H, 5 * H, 6 * H, 7 * H, 8 * H, 9 * H, 10 * H, 11 * H, 12 * H, 13 * H, 14 * H,
];
