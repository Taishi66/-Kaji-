use kaji::replay::clock::{FixedClock, PromptClock, RealClock};

#[test]
fn fixed_clock_returns_its_value() {
    let c = FixedClock::new("2026-08-27 10:00 +02:00".to_string());
    assert_eq!(c.prompt_timestamp(), "2026-08-27 10:00 +02:00");
}

#[test]
fn real_clock_matches_prompt_format() {
    let ts = RealClock.prompt_timestamp();
    // "YYYY-MM-DD HH:00 +TZ" — 4-2-2 date, heure pilée à :00
    assert!(ts.len() >= 16, "{ts}");
    assert_eq!(ts.chars().nth(4), Some('-'));
    assert!(ts.contains(":00 "), "{ts}");
}
