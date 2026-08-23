pub(crate) fn cached_sql(prefix: &str, build: fn(&str) -> String) -> &'static str {
    use std::sync::{Mutex, OnceLock};
    // Keyed by (prefix, build) — the prefix alone is NOT a unique key: four
    // statements (the depth-1 CTE, custom properties, the two display-name
    // lookups) share the table prefix, and a prefix-only key made the first
    // caller's SQL leak into the others (wrong statement + own binds → the
    // 2026-08-14 desync errors and the ColumnNotFound panic).
    static CACHE: OnceLock<
        Mutex<std::collections::HashMap<(String, fn(&str) -> String), &'static str>>,
    > = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let key = (prefix.to_string(), build);
    if let Some(s) = cache.lock().expect("sql cache lock").get(&key) {
        return *s;
    }
    let s: &'static str = Box::leak(build(prefix).into_boxed_str());
    cache.lock().expect("sql cache lock").insert(key, s);
    s
}
