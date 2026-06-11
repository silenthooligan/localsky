// Day-bucket math shared by the daily history charts (zone detail,
// History page, mobile zone detail, per-zone history strip). Pulled out
// so the boundary behavior is testable in one place.

/// How many local calendar days back a run started: 0 = today,
/// 1 = yesterday. `today_mid` is the epoch of local midnight today.
///
/// Truncating division `(today_mid - start) / 86_400` rounded toward
/// zero, so every run in the 24h window straddling midnight landed in
/// bucket 0 and an 11 PM run yesterday counted as today. Floor division
/// on the signed day offset respects the midnight boundary instead.
/// A negative result means the run starts after today (clock skew or a
/// scheduled future entry); callers clamp those into today with .max(0).
pub fn days_back(today_mid: i64, start_epoch: i64) -> i64 {
    -((start_epoch - today_mid).div_euclid(86_400))
}

#[cfg(test)]
mod tests {
    use super::days_back;

    const DAY: i64 = 86_400;
    // Arbitrary midnight-aligned epoch standing in for local midnight.
    const TODAY_MID: i64 = 1_700_006_400;

    #[test]
    fn run_at_6am_today_is_today() {
        assert_eq!(days_back(TODAY_MID, TODAY_MID + 6 * 3_600), 0);
    }

    #[test]
    fn run_at_11pm_yesterday_is_yesterday() {
        assert_eq!(days_back(TODAY_MID, TODAY_MID - 3_600), 1);
    }

    #[test]
    fn midnight_boundaries_split_cleanly() {
        assert_eq!(days_back(TODAY_MID, TODAY_MID), 0);
        assert_eq!(days_back(TODAY_MID, TODAY_MID - 1), 1);
        assert_eq!(days_back(TODAY_MID, TODAY_MID - DAY), 1);
        assert_eq!(days_back(TODAY_MID, TODAY_MID - DAY - 1), 2);
        assert_eq!(days_back(TODAY_MID, TODAY_MID + DAY - 1), 0);
    }

    #[test]
    fn genuinely_future_run_goes_negative_for_caller_clamp() {
        assert_eq!(days_back(TODAY_MID, TODAY_MID + DAY + 3_600), -1);
    }
}
