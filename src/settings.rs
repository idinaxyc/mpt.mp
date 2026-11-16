//! Пользовательские настройки: звук, плагины, оформление.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Состояние одного плагина: включён ли он и какие значения настроек выбрал пользователь.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSettings {
    pub enabled: bool,
    /// Права, которые пользователь подтвердил при включении. Если плагин после
    /// обновления просит больше — автозапуск отменяется до повторного включения.
    pub granted: Vec<String>,
    pub config: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub volume: f32,
    /// Плагины по id.
    pub plugins: HashMap<String, PluginSettings>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 0.8,
            plugins: HashMap::new(),
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        crate::model::data_dir().join("settings.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        let text = std::fs::read_to_string(&path).ok();
        let parsed = text
            .as_deref()
            .and_then(|t| serde_json::from_str::<Self>(t).ok());
        if parsed.is_none() && text.is_some() {
            // Файл есть, но не читается: оставляем копию, иначе следующее сохранение
            // затёрло бы настройки плагинов (в том числе токены).
            let _ = std::fs::copy(&path, path.with_extension("json.bad"));
        }
        let mut settings = parsed.unwrap_or_default();
        settings.normalize();
        settings
    }

    pub fn save(&self) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::model::write_atomic(&Self::path(), text.as_bytes())
    }

    fn normalize(&mut self) {
        self.volume = self.volume.clamp(0.0, 1.0);
    }
}
