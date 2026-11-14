use super::feature::Feature;
use super::featureAssembly::IFeatureAssemblyService;
use super::featureRegistry::{FeatureRecipe, get_feature_recipes};

pub const FEATURE_ASSEMBLY_SCOPE: &str = "app";
pub const FEATURE_ASSEMBLY_ACTIVATION: &str = "onScopeCreated";

#[derive(Debug, Clone, Copy, Default)]
pub struct FeatureAssemblyService;

impl FeatureAssemblyService {
    pub fn new() -> Self {
        Self
    }

    pub fn assemble(&self, recipes: &[FeatureRecipe]) -> Vec<Feature> {
        recipes.iter().map(|recipe| recipe()).collect()
    }

    pub fn assemble_registered(&self) -> Vec<Feature> {
        self.assemble(&get_feature_recipes())
    }
}

impl IFeatureAssemblyService for FeatureAssemblyService {}
