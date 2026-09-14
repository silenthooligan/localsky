//! Coverage-aware precipitation integration shared by forecast producers and
//! consumers. An interval total is spread uniformly only within its declared
//! interval; absent evidence never contributes a dry interval.

pub(crate) fn valid_amount(amount: f64) -> bool {
    amount.is_finite() && amount >= 0.0
}

/// Sum depths over exactly `[start, end)`. The input total covers its own
/// `[from, to)` interval. Gaps, invalid amounts and overlapping declarations
/// make the answer unknown rather than creating dry weather or double rain.
pub(crate) fn total_over(
    intervals: impl IntoIterator<Item = (i64, i64, Option<f64>)>,
    start: i64,
    end: i64,
) -> Option<f64> {
    if end < start {
        return None;
    }
    if end == start {
        return Some(0.0);
    }
    let mut rows: Vec<_> = intervals
        .into_iter()
        .filter(|(from, to, _)| *from < end && *to > start)
        .collect();
    rows.sort_by_key(|(from, to, _)| (*from, *to));
    let mut covered_until = start;
    let mut total = 0.0;
    for (from, to, amount) in rows {
        let amount = amount.filter(|v| valid_amount(*v))?;
        let left = from.max(start);
        let right = to.min(end);
        if to <= from || left != covered_until {
            return None;
        }
        total += amount * (right - left) as f64 / (to - from) as f64;
        covered_until = right;
    }
    (covered_until == end && total.is_finite()).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_distinguishes_dry_partial_missing_and_duplicate_intervals() {
        assert_eq!(total_over([(0, 3600, Some(0.0))], 0, 3600), Some(0.0));
        assert_eq!(total_over([(0, 3600, None)], 0, 3600), None);
        assert_eq!(total_over([(0, 3600, Some(0.0))], 0, 7200), None);
        assert_eq!(
            total_over([(0, 3600, Some(0.1)), (0, 3600, Some(0.1))], 0, 3600),
            None
        );
        assert_eq!(total_over([(0, 3600, Some(-999.0))], 0, 3600), None);
        assert_eq!(total_over([(0, 3600, Some(f64::NAN))], 0, 3600), None);
    }

    #[test]
    fn exact_window_excludes_elapsed_rain_and_prorates_boundaries() {
        let rows = [
            (0, 3600, Some(10.0)),
            (3600, 7200, Some(1.0)),
            (7200, 10800, Some(2.0)),
        ];
        assert_eq!(total_over(rows, 5400, 9000), Some(1.5));
        assert_eq!(total_over(rows, 10800, 14400), None);
    }
}
