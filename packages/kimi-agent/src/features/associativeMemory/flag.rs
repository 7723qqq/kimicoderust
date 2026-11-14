use std::sync::LazyLock;

pub const ASSOCIATIVE_MEMORY_FLAG_ID: &str = "associative_memory";
pub const ASSOCIATIVE_MEMORY_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_ASSOCIATIVE_MEMORY";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagSurface {
    Core,
    Tui,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagDefinition {
    pub id: String,
    pub title: String,
    pub description: String,
    pub env: String,
    pub default: bool,
    pub surface: FlagSurface,
}

pub fn associative_memory_flag() -> FlagDefinition {
    FlagDefinition {
        id: ASSOCIATIVE_MEMORY_FLAG_ID.to_string(),
        title: "Associative memory".to_string(),
        description:
            "Encode experience into atoms, learn weighted associations, and inject related memories on each turn."
                .to_string(),
        env: ASSOCIATIVE_MEMORY_FLAG_ENV.to_string(),
        default: false,
        surface: FlagSurface::Both,
    }
}

static CONTRIBUTED_FLAGS: LazyLock<Vec<FlagDefinition>> = LazyLock::new(|| {
    let mut flags = Vec::new();
    register_flag_definition(&mut flags, associative_memory_flag());
    flags
});

pub fn register_flag_definition(flags: &mut Vec<FlagDefinition>, definition: FlagDefinition) {
    flags.push(definition);
}

pub fn get_contributed_flags() -> &'static [FlagDefinition] {
    &CONTRIBUTED_FLAGS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_matches_v2_field_by_field() {
        let flag = associative_memory_flag();
        assert_eq!(flag.id, "associative_memory");
        assert_eq!(flag.title, "Associative memory");
        assert_eq!(
            flag.description,
            "Encode experience into atoms, learn weighted associations, and inject related memories on each turn."
        );
        assert_eq!(flag.env, "KIMI_CODE_EXPERIMENTAL_ASSOCIATIVE_MEMORY");
        assert!(!flag.default);
        assert_eq!(flag.surface, FlagSurface::Both);
    }

    #[test]
    fn the_definition_is_registered_exactly_once() {
        let flags = get_contributed_flags();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].id, ASSOCIATIVE_MEMORY_FLAG_ID);
    }

    #[test]
    fn registering_appends_to_the_supplied_list() {
        let mut flags = vec![associative_memory_flag()];
        register_flag_definition(&mut flags, associative_memory_flag());
        assert_eq!(flags.len(), 2);
    }
}
