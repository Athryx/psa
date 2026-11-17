//! Display-independent calculations over cumulative agent usage.

use llm_api::api::Usage;

/// Fraction of input tokens served from cache, when both cumulative counts are
/// known and consistent. An empty input total has no defined hit rate.
///
/// Usage includes inherited fork history and compaction; it must not be summed
/// across forks to estimate provider-wide usage.
pub fn cache_hit_rate(usage: &Usage) -> Option<f64> {
    let input = usage.input_tokens?;
    let cached = usage.cached_input_tokens?;
    if input == 0 || cached > input {
        return None;
    }
    Some(cached as f64 / input as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_rate_requires_known_consistent_nonzero_input() {
        let mut usage = Usage::zero();
        assert_eq!(cache_hit_rate(&usage), None);
        usage.input_tokens = Some(100);
        assert_eq!(cache_hit_rate(&usage), Some(0.0));
        usage.cached_input_tokens = Some(25);
        assert_eq!(cache_hit_rate(&usage), Some(0.25));
        usage.cached_input_tokens = Some(100);
        assert_eq!(cache_hit_rate(&usage), Some(1.0));
        usage.cached_input_tokens = Some(101);
        assert_eq!(cache_hit_rate(&usage), None);
        usage.cached_input_tokens = None;
        assert_eq!(cache_hit_rate(&usage), None);
        usage.input_tokens = None;
        usage.cached_input_tokens = Some(25);
        assert_eq!(cache_hit_rate(&usage), None);
    }
}
