pub fn error_message(error: impl ToString) -> String {
    error.to_string()
}
