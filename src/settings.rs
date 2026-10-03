use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Appearance {
    pub transparency: f32,
    pub blur: f32,
    pub theme: u8,
}
impl Default for Appearance {
    fn default() -> Self {
        Self {
            transparency: 55.0,
            blur: 25.0,
            theme: 0,
        }
    }
}
impl Appearance {
    pub fn path() -> PathBuf {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(if cfg!(feature = "testing-ui") {
                "FeathercutTesting/preferences.json"
            } else {
                "Feathercut/preferences.json"
            })
    }
    pub fn load(path: &Path) -> Self {
        let Some(value) = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        else {
            return Self::default();
        };
        let mut appearance: Self = serde_json::from_value(value.clone()).unwrap_or_default();
        // Old toggle files use the new mild-glass default, retaining an explicit solid choice.
        if value.get("transparency").is_none()
            && value.get("desktop_translucency").and_then(|v| v.as_bool()) == Some(false)
        {
            appearance.transparency = 0.0;
        }
        appearance.transparency = Self::bounded(appearance.transparency, 55.0);
        appearance.blur = Self::bounded(appearance.blur, 25.0);
        if appearance.theme > 1 {
            appearance.theme = 0;
        }
        appearance
    }
    fn bounded(value: f32, default: f32) -> f32 {
        if value.is_finite() {
            value.clamp(0.0, 100.0)
        } else {
            default
        }
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, bytes).map_err(|e| e.to_string())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn appearance_preferences_round_trip_migrate_and_bound_sliders() {
        let path = PathBuf::from("target/ui-verification/preferences-test.json");
        let appearance = Appearance {
            transparency: 42.0,
            blur: 17.0,
            theme: 1,
        };
        appearance.save(&path).unwrap();
        assert_eq!(Appearance::load(&path), appearance);
        std::fs::write(&path, b"invalid").unwrap();
        assert_eq!(Appearance::load(&path), Appearance::default());
        std::fs::write(&path, br#"{"theme":9}"#).unwrap();
        assert_eq!(Appearance::load(&path).theme, 0);
        std::fs::write(&path, br#"{"desktop_translucency":false}"#).unwrap();
        assert_eq!(Appearance::load(&path).transparency, 0.0);
        std::fs::write(
            &path,
            br#"{"desktop_translucency":true,"blur_background":false}"#,
        )
        .unwrap();
        assert_eq!(Appearance::load(&path), Appearance::default());
        std::fs::write(&path, br#"{"transparency":120,"blur":-5}"#).unwrap();
        assert_eq!(
            Appearance::load(&path),
            Appearance {
                transparency: 100.0,
                blur: 0.0,
                theme: 0
            }
        );
        std::fs::remove_file(&path).unwrap();
    }
}
