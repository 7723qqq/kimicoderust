pub fn new_message_id() -> String {
    format!("msg_{}", ulid::Ulid::new())
}
