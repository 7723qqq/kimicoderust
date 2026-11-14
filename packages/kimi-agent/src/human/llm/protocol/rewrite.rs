#[derive(Debug, Clone)]
pub struct Rewrite<T> {
    pub consumed: usize,
    pub replacement: Vec<T>,
}

pub trait Pattern<T>: Send + Sync {
    fn name(&self) -> &str;
    fn rewrite(&self, items: &[T], index: usize) -> Option<Rewrite<T>>;
}

pub fn apply_patterns<T: Clone>(items: &[T], patterns: &[&dyn Pattern<T>]) -> Vec<T> {
    let mut current = items.to_vec();
    for pattern in patterns {
        let mut next = Vec::new();
        let mut index = 0usize;
        while index < current.len() {
            match pattern.rewrite(&current, index) {
                None => {
                    next.push(current[index].clone());
                    index += 1;
                }
                Some(rewrite) => {
                    next.extend(rewrite.replacement);
                    index += rewrite.consumed;
                }
            }
        }
        current = next;
    }
    current
}
