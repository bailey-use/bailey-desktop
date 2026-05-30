/// Strip dashes from a session id and clip to `width` chars. Preserves
/// a leading `T-` source tag literally so the row stays grep-able.
pub fn compact_id(id: &str, width: usize) -> String {
    if let Some(rest) = id.strip_prefix("T-") {
        let take = width.saturating_sub(2);
        let no_dashes: String = rest.chars().filter(|c| *c != '-').collect();
        format!("T-{}", no_dashes.chars().take(take).collect::<String>())
    } else {
        id.chars().filter(|c| *c != '-').take(width).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_dashes_and_clips() {
        assert_eq!(
            compact_id("1335c631-9147-4e2d-a2dc-foo", 12),
            "1335c63191474".chars().take(12).collect::<String>()
        );
    }

    #[test]
    fn preserves_amp_prefix() {
        assert_eq!(
            compact_id("T-019e0f72-7e6c-72be-abcd", 12),
            format!(
                "T-{}",
                "019e0f727e6c72be".chars().take(10).collect::<String>()
            )
        );
    }

    #[test]
    fn handles_short_ids() {
        assert_eq!(compact_id("abc", 12), "abc");
        assert_eq!(compact_id("T-x", 12), "T-x");
    }
}
