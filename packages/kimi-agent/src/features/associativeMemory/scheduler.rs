pub fn rank_by_need_gain<T, Need, Gain, Tie>(
    items: &[T],
    need_of: Need,
    gain_of: Gain,
    tie: Tie,
) -> Vec<T>
where
    T: Clone,
    Need: Fn(&T) -> f64,
    Gain: Fn(&T) -> f64,
    Tie: Fn(&T, &T) -> std::cmp::Ordering,
{
    let mut ranked: Vec<(T, f64)> = items
        .iter()
        .map(|item| {
            let score = need_of(item) * gain_of(item);
            (item.clone(), score)
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| tie(&left.0, &right.0))
    });
    ranked.into_iter().map(|(item, _)| item).collect()
}

pub fn interleave<T>(ranked: &[T], limit: usize, strata: usize) -> Vec<T>
where
    T: Clone,
{
    if ranked.is_empty() || limit == 0 || strata == 0 {
        return Vec::new();
    }
    let mut tiers: Vec<Vec<T>> = vec![Vec::new(); strata];
    for (index, item) in ranked.iter().enumerate() {
        let tier = ((index * strata) / ranked.len()).min(strata - 1);
        tiers[tier].push(item.clone());
    }
    let mut out: Vec<T> = Vec::new();
    let mut cursor = 0_usize;
    while out.len() < limit.min(ranked.len()) {
        let tier = &mut tiers[cursor % strata];
        if let Some(item) = tier.first().cloned() {
            tier.remove(0);
            out.push(item);
        }
        cursor += 1;
        if tiers.iter().all(|candidate| candidate.is_empty()) {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_id(left: &(&str, i64), right: &(&str, i64)) -> std::cmp::Ordering {
        left.0.cmp(right.0)
    }

    #[test]
    fn ranking_multiplies_need_by_gain_and_breaks_ties() {
        let items = vec![("a", 1_i64), ("b", 2), ("c", 3)];
        let ranked = rank_by_need_gain(
            &items,
            |item| item.1 as f64,
            |item| if item.0 == "a" { 9.0 } else { 1.0 },
            by_id,
        );
        assert_eq!(ranked, vec![("a", 1), ("c", 3), ("b", 2)]);
    }

    #[test]
    fn ties_fall_back_to_the_supplied_comparator() {
        let items = vec![("b", 2_i64), ("a", 2)];
        let ranked = rank_by_need_gain(&items, |item| item.1 as f64, |_| 1.0, by_id);
        assert_eq!(ranked, vec![("a", 2), ("b", 2)]);
    }

    #[test]
    fn interleaving_round_robins_the_tiers() {
        let ranked: Vec<i32> = (0..9).collect();
        let out = interleave(&ranked, 6, 3);
        assert_eq!(out, vec![0, 3, 6, 1, 4, 7]);
    }

    #[test]
    fn interleaving_is_bounded_by_limit_and_input() {
        let ranked: Vec<i32> = (0..5).collect();
        assert_eq!(interleave(&ranked, 3, 2), vec![0, 3, 1]);
        assert_eq!(interleave(&ranked, 99, 2).len(), 5);
        assert!(interleave(&ranked, 0, 2).is_empty());
        assert!(interleave(&ranked, 3, 0).is_empty());
        assert!(interleave::<i32>(&[], 3, 2).is_empty());
    }

    #[test]
    fn a_single_item_input_terminates() {
        let ranked = vec![42_i32];
        assert_eq!(interleave(&ranked, 5, 4), vec![42]);
    }

    #[test]
    fn more_strata_than_items_still_terminates() {
        let ranked = vec![1_i32, 2];
        assert_eq!(interleave(&ranked, 2, 5), vec![1, 2]);
    }
}
