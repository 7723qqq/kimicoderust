use std::sync::{LazyLock, RwLock};

use super::feature::Feature;

pub type FeatureRecipe = fn() -> Feature;

static FEATURE_RECIPES: LazyLock<RwLock<Vec<FeatureRecipe>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

pub fn register_feature(recipe: FeatureRecipe) {
    FEATURE_RECIPES.write().unwrap().push(recipe);
}

pub fn get_feature_recipes() -> Vec<FeatureRecipe> {
    FEATURE_RECIPES.read().unwrap().clone()
}

pub fn clear_feature_recipes_for_tests() {
    FEATURE_RECIPES.write().unwrap().clear();
}
