//! Display-independent calculations over cumulative agent usage.

use llm_api::api::Usage;

/// Fraction of cacheable input served from cache, when both counts are known
/// and consistent. Cacheable input is what each request repeated from the
/// previous one (the agent's `cacheable_input_tokens`), so new input no cache
/// could hold does not lower the rate. No cacheable input has no defined rate.
///
/// Usage includes inherited fork history and compaction; it must not be summed
/// across forks to estimate provider-wide usage.
pub fn cache_hit_rate(usage: &Usage, cacheable_input_tokens: Option<u64>) -> Option<f64> {
    let cacheable = cacheable_input_tokens?;
    let cached = usage.cached_input_tokens?;
    if cacheable == 0 || cached > cacheable {
        return None;
    }
    Some(cached as f64 / cacheable as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_rate_requires_known_consistent_nonzero_cacheable_input() {
        let mut usage = Usage::zero();
        usage.input_tokens = Some(1000);
        assert_eq!(cache_hit_rate(&usage, Some(0)), None);
        assert_eq!(cache_hit_rate(&usage, Some(100)), Some(0.0));
        usage.cached_input_tokens = Some(25);
        assert_eq!(cache_hit_rate(&usage, Some(100)), Some(0.25));
        usage.cached_input_tokens = Some(100);
        assert_eq!(cache_hit_rate(&usage, Some(100)), Some(1.0));
        assert_eq!(cache_hit_rate(&usage, None), None);
        usage.cached_input_tokens = Some(101);
        assert_eq!(cache_hit_rate(&usage, Some(100)), None);
        usage.cached_input_tokens = None;
        assert_eq!(cache_hit_rate(&usage, Some(100)), None);
    }
}
