// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::env;
use std::path::PathBuf;

use jujutsu_lib::settings::UserSettings;

fn config_path() -> Option<PathBuf> {
    if let Ok(config_path) = env::var("JJ_CONFIG") {
        // TODO: We should probably support colon-separated (std::env::split_paths)
        // paths here
        Some(PathBuf::from(config_path))
    } else {
        // TODO: Should we drop the final `/config.toml` and read all files in the
        // directory?
        dirs::config_dir().map(|config_dir| config_dir.join("jj").join("config.toml"))
    }
}

/// Environment variables that should be overridden by config values
fn env_base() -> config::Config {
    let mut builder = config::Config::builder();
    if let Ok(value) = env::var("EDITOR") {
        builder = builder.set_override("ui.editor", value).unwrap();
    }
    builder.build().unwrap()
}

/// Environment variables that override config values
fn env_overrides() -> config::Config {
    let mut builder = config::Config::builder();
    if let Ok(value) = env::var("JJ_USER") {
        builder = builder.set_override("user.name", value).unwrap();
    }
    if let Ok(value) = env::var("JJ_EMAIL") {
        builder = builder.set_override("user.email", value).unwrap();
    }
    if let Ok(value) = env::var("JJ_TIMESTAMP") {
        builder = builder.set_override("user.timestamp", value).unwrap();
    }
    if let Ok(value) = env::var("JJ_EDITOR") {
        builder = builder.set_override("ui.editor", value).unwrap();
    }
    builder.build().unwrap()
}

pub fn read_config() -> Result<UserSettings, config::ConfigError> {
    let mut config_builder = config::Config::builder().add_source(env_base());

    if let Some(config_path) = config_path() {
        let mut files = vec![];
        if config_path.is_dir() {
            if let Ok(read_dir) = config_path.read_dir() {
                // TODO: Walk the directory recursively?
                for dir_entry in read_dir.flatten() {
                    let path = dir_entry.path();
                    if path.is_file() {
                        files.push(path);
                    }
                }
            }
            files.sort();
        } else {
            files.push(config_path);
        }
        for file in files {
            // TODO: Accept other formats and/or accept only certain file extensions?
            config_builder = config_builder.add_source(
                config::File::from(file)
                    .required(false)
                    .format(config::FileFormat::Toml),
            );
        }
    };

    let config = config_builder.add_source(env_overrides()).build()?;
    Ok(UserSettings::from_config(config))
}