use std::ffi::OsString;

const ASYLUM_PREFIX: &str = "ASYLUM_";
const ZED_PREFIX: &str = "ZED_";

/// Makes every `ZED_*` environment variable also visible as `ASYLUM_*`, so setups written for
/// Zed keep working with Asylum, which only reads the `ASYLUM_*` names. An `ASYLUM_*` variable
/// that is already set keeps its value.
///
/// # Safety
///
/// Changes the process environment, so it must run before the process starts other threads.
pub unsafe fn adopt_zed_environment() {
    let renamed: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let asylum_key = OsString::from(asylum_name(key.to_str()?)?);
            std::env::var_os(&asylum_key)
                .is_none()
                .then_some((asylum_key, value))
        })
        .collect();
    for (key, value) in renamed {
        // SAFETY: the caller guarantees no other thread is running.
        unsafe { std::env::set_var(key, value) };
    }
}

/// Returns `name` with a `ZED_` prefix replaced by `ASYLUM_`.
pub fn asylum_name(name: &str) -> Option<String> {
    name.strip_prefix(ZED_PREFIX)
        .map(|suffix| format!("{ASYLUM_PREFIX}{suffix}"))
}

/// Returns `name` with an `ASYLUM_` prefix replaced by `ZED_`, the name Zed used for the same
/// variable.
pub fn zed_name(name: &str) -> Option<String> {
    name.strip_prefix(ASYLUM_PREFIX)
        .map(|suffix| format!("{ZED_PREFIX}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_names() {
        assert_eq!(asylum_name("ZED_FILE").as_deref(), Some("ASYLUM_FILE"));
        assert_eq!(zed_name("ASYLUM_FILE").as_deref(), Some("ZED_FILE"));
        assert_eq!(asylum_name("PATH"), None);
        assert_eq!(zed_name("PATH"), None);
    }
}
