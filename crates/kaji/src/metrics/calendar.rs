//! Bornes calendaires locales — jour, semaine (lundi), mois — exprimées en
//! secondes epoch pour interroger `usage_ledger.created_timestamp`.
//!
//! `now` est toujours un paramètre : la lecture d'horloge appartient à
//! l'appelant (hors boucle agent pour `/cost` et `kaji metrics`), ce qui rend
//! les bornes testables sur des dates fixes — minuit, dimanche/lundi, 31/1er.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone};

/// Intervalle semi-ouvert `[start, end)` en secondes epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: i64,
    pub end: i64,
}

impl Span {
    pub fn sliding(now: DateTime<Local>, back_secs: i64) -> Self {
        let end = now.timestamp();
        Span {
            start: end - back_secs,
            end,
        }
    }
}

/// Minuit local du jour `date`. Sous un saut d'heure d'été qui supprime
/// minuit, la première heure existante du jour fait office de borne.
fn local_midnight(date: NaiveDate) -> i64 {
    for hour in 0..24 {
        let naive = date
            .and_hms_opt(hour, 0, 0)
            .expect("heure valide dans 0..24");
        if let Some(dt) = Local.from_local_datetime(&naive).earliest() {
            return dt.timestamp();
        }
    }
    Local
        .from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("minuit valide"))
        .timestamp()
}

pub fn day_span(now: DateTime<Local>) -> Span {
    let today = now.date_naive();
    Span {
        start: local_midnight(today),
        end: local_midnight(today + Duration::days(1)),
    }
}

/// Semaine ISO : du lundi 00:00 au lundi suivant 00:00.
pub fn week_span(now: DateTime<Local>) -> Span {
    let today = now.date_naive();
    let back = i64::from(today.weekday().num_days_from_monday());
    let monday = today - Duration::days(back);
    Span {
        start: local_midnight(monday),
        end: local_midnight(monday + Duration::days(7)),
    }
}

pub fn month_span(now: DateTime<Local>) -> Span {
    let today = now.date_naive();
    let first =
        NaiveDate::from_ymd_opt(today.year(), today.month(), 1).expect("1er du mois valide");
    let next_first = if today.month() == 12 {
        NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(today.year(), today.month() + 1, 1)
    }
    .expect("1er du mois suivant valide");
    Span {
        start: local_midnight(first),
        end: local_midnight(next_first),
    }
}

pub fn days_in_month(now: DateTime<Local>) -> u32 {
    let today = now.date_naive();
    let next_first = if today.month() == 12 {
        NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(today.year(), today.month() + 1, 1)
    }
    .expect("1er du mois suivant valide");
    (next_first - NaiveDate::from_ymd_opt(today.year(), today.month(), 1).expect("1er du mois"))
        .num_days() as u32
}

/// Rang 1-basé du jour courant dans son mois.
pub fn day_of_month(now: DateTime<Local>) -> u32 {
    now.date_naive().day()
}

/// Clé `YYYY-MM-DD` du jour local d'un instant epoch — même fuseau que
/// [`day_span`], donc une ligne du ledger tombe toujours dans le jour qui la
/// contient. `None` pour un timestamp hors de la plage représentable.
pub fn local_day_key(timestamp: i64) -> Option<String> {
    DateTime::from_timestamp(timestamp, 0)
        .map(|instant| instant.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(y, m, d)
                    .unwrap()
                    .and_hms_opt(h, min, 0)
                    .unwrap(),
            )
            .earliest()
            .unwrap()
    }

    #[test]
    fn day_span_covers_local_midnight_to_next_midnight() {
        let span = day_span(at(2026, 9, 5, 14, 30));
        assert_eq!(span.start, at(2026, 9, 5, 0, 0).timestamp());
        assert_eq!(span.end, at(2026, 9, 6, 0, 0).timestamp());
    }

    #[test]
    fn day_span_at_23h59_and_at_00h00_are_adjacent_days() {
        let late = day_span(at(2026, 9, 5, 23, 59));
        let early = day_span(at(2026, 9, 6, 0, 0));
        assert_eq!(late.end, early.start, "les jours se touchent sans trou");
        assert_ne!(late.start, early.start, "et ne se confondent pas");
    }

    #[test]
    fn week_span_starts_monday_even_when_asked_on_sunday() {
        // 2026-09-06 est un dimanche, 2026-09-07 le lundi suivant.
        let sunday = week_span(at(2026, 9, 6, 12, 0));
        assert_eq!(sunday.start, at(2026, 8, 31, 0, 0).timestamp());
        assert_eq!(sunday.end, at(2026, 9, 7, 0, 0).timestamp());

        let monday = week_span(at(2026, 9, 7, 0, 0));
        assert_eq!(monday.start, at(2026, 9, 7, 0, 0).timestamp());
        assert_eq!(
            sunday.end, monday.start,
            "dimanche 23:59 et lundi 00:00 tombent dans deux semaines"
        );
    }

    #[test]
    fn month_span_wraps_from_december_to_january() {
        let span = month_span(at(2026, 12, 31, 23, 30));
        assert_eq!(span.start, at(2026, 12, 1, 0, 0).timestamp());
        assert_eq!(span.end, at(2027, 1, 1, 0, 0).timestamp());
    }

    #[test]
    fn month_span_on_the_31st_and_the_1st_are_different_months() {
        let last = month_span(at(2026, 8, 31, 23, 59));
        let first = month_span(at(2026, 9, 1, 0, 0));
        assert_eq!(last.end, first.start);
        assert_eq!(last.start, at(2026, 8, 1, 0, 0).timestamp());
    }

    #[test]
    fn days_in_month_counts_february_leap_years() {
        assert_eq!(days_in_month(at(2026, 2, 10, 9, 0)), 28);
        assert_eq!(days_in_month(at(2028, 2, 10, 9, 0)), 29);
        assert_eq!(days_in_month(at(2026, 9, 5, 9, 0)), 30);
        assert_eq!(days_in_month(at(2026, 12, 5, 9, 0)), 31);
    }

    #[test]
    fn day_of_month_is_one_based() {
        assert_eq!(day_of_month(at(2026, 9, 1, 0, 0)), 1);
        assert_eq!(day_of_month(at(2026, 9, 30, 23, 59)), 30);
    }

    #[test]
    fn local_day_key_agrees_with_the_day_span_that_contains_it() {
        let noon = at(2026, 9, 5, 12, 0);
        assert_eq!(local_day_key(noon.timestamp()).unwrap(), "2026-09-05");

        let span = day_span(noon);
        assert_eq!(local_day_key(span.start).unwrap(), "2026-09-05");
        assert_eq!(
            local_day_key(span.end - 1).unwrap(),
            "2026-09-05",
            "la dernière seconde du jour reste dans le jour"
        );
        assert_eq!(
            local_day_key(span.end).unwrap(),
            "2026-09-06",
            "la borne haute appartient au jour suivant"
        );
    }

    #[test]
    fn local_day_key_declines_an_unrepresentable_timestamp() {
        assert_eq!(local_day_key(i64::MAX), None);
    }
}
