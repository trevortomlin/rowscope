//! Icons compiled into the binary (see build.rs).

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub struct Assets;

const ICONS: &[(&str, &str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/icons.rs"));

pub fn icon(name: &str) -> SharedString {
    // Requested every frame, so hand out the static path instead of allocating.
    match ICONS.iter().find(|(k, _, _)| *k == name) {
        Some((_, path, _)) => SharedString::new_static(path),
        None => SharedString::new_static("icons/file.svg"),
    }
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let name = path
            .strip_prefix("icons/")
            .and_then(|p| p.strip_suffix(".svg"));
        Ok(name
            .and_then(|n| ICONS.iter().find(|(k, _, _)| *k == n))
            .map(|(_, _, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|(_, p, _)| SharedString::new_static(p))
            .filter(|p| p.starts_with(path))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn icons_are_embedded() {
        assert!(super::ICONS.iter().any(|(k, _, _)| *k == "table"));
        assert!(super::ICONS.len() >= 20);
    }
}
