use serde_json::Value;

/// Whether `args` is an object whose keys are all in `allowed`.
pub fn only(args: &Value, allowed: &[&str]) -> bool {
    args.as_object()
        .is_some_and(|object| object.keys().all(|key| allowed.contains(&key.as_str())))
}
