pub fn is_record(value: &serde_json::Value) -> bool {
    value.is_object()
}
