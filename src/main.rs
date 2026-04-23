use std::error::Error;
use std::path::PathBuf;

use chameleon::{KeyboardFilter, KeyboardLayout};
use serde::Deserialize;

const DEFAULT_CONFIG: &str = r#"# chameleon configuration
# Layouts accepted: EnglishUS, EnglishUK, SpanishLatinAmerica, SpanishSpain,
# French, German, PortugueseBrazil, Italian, or a raw 8-digit KLID string.

default_layout = "SpanishLatinAmerica"

# Add one [[keyboards]] block per device you want to map.
# `id` is the `VID_xxxx&PID_xxxx` substring of the device's symbolic link.
# `alias` is optional and only used in logs.
#
# [[keyboards]]
# id = "VID_258A&PID_002A"
# alias = "Akko"
# layout = "EnglishUS"
"#;

#[derive(Debug, Deserialize)]
struct Config {
    default_layout: KeyboardLayout,
    #[serde(default)]
    keyboards: Vec<KeyboardSpec>,
}

#[derive(Debug, Deserialize)]
struct KeyboardSpec {
    id: String,
    #[serde(default)]
    alias: Option<String>,
    layout: KeyboardLayout,
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("chameleon")
        .join("config.toml")
}

fn load_or_create_config() -> Result<Config, Box<dyn Error>> {
    let path = config_path();
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, DEFAULT_CONFIG)?;
        return Err(format!(
            "config created at {} — edit it and rerun",
            path.display()
        )
        .into());
    }
    let text = std::fs::read_to_string(&path)?;
    let config: Config = toml::from_str(&text)?;
    Ok(config)
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    let config = load_or_create_config()?;

    let mut builder = KeyboardFilter::builder().default_layout(config.default_layout);
    for kb in config.keyboards {
        builder = builder.on_connect(kb.id, kb.alias, kb.layout);
    }
    let filter = builder.build()?;

    let _watcher = filter.watch()?;

    std::thread::park();
    Ok(())
}
