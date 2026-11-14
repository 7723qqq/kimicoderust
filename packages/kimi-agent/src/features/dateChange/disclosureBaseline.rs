use super::date_change_service::DateInjectionDisclosure;

pub trait DisclosureBaselineCandidate: Send + Sync {
    fn render_generation(&self) -> u32;
}

impl DisclosureBaselineCandidate for DateInjectionDisclosure {
    fn render_generation(&self) -> u32 {
        self.render_generation
    }
}

pub fn pick_disclosure_baseline<'a, T: DisclosureBaselineCandidate>(
    candidates: &[Option<&'a T>],
) -> Option<&'a T> {
    let mut winner: Option<&'a T> = None;
    for candidate in candidates.iter().filter_map(|candidate| *candidate) {
        let take = match winner {
            None => true,
            Some(current) => candidate.render_generation() > current.render_generation(),
        };
        if take {
            winner = Some(candidate);
        }
    }
    winner
}
