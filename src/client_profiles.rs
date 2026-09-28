//! Generate client connection files from Kinetix's public endpoint and a
//! client-visible model or Route. These files configure clients to call only
//! Kinetix; upstream provider credentials are never part of the profile.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{json, Value};

const KEY_ENV: &str = "KINETIX_API_KEY";
const KEY_PLACEHOLDER: &str = "sk-kinetix-<paste-your-key>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientApp {
    Pi,
    ClaudeCode,
    Codex,
    OpenCode,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileFile {
    pub filename: &'static str,
    pub destination: Option<&'static str>,
    pub content_type: &'static str,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeneratedProfile {
    pub client: ClientApp,
    pub model: String,
    pub public_base_url: String,
    pub files: Vec<ProfileFile>,
}

pub fn generate(
    client: ClientApp,
    public_base_url: &str,
    model: &str,
    api_key: Option<&str>,
) -> GeneratedProfile {
    let root = endpoint_root(public_base_url);
    let openai_base_url = format!("{root}/v1");
    let api_key = api_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .unwrap_or(KEY_PLACEHOLDER);

    let files = match client {
        ClientApp::Pi => pi_files(&openai_base_url, model, api_key),
        ClientApp::ClaudeCode => vec![claude_code_file(&root, model, api_key)],
        ClientApp::Codex => codex_files(&openai_base_url, model, api_key),
        ClientApp::OpenCode => open_code_files(&openai_base_url, model, api_key),
    };

    GeneratedProfile {
        client,
        model: model.to_string(),
        public_base_url: public_base_url.trim_end_matches('/').to_string(),
        files,
    }
}

/// Remove a terminal `/v1` from the configured base so each client can receive
/// the path shape its own API client expects.
fn endpoint_root(public_base_url: &str) -> String {
    let trimmed = public_base_url.trim().trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

fn json_file(filename: &'static str, destination: &'static str, content: Value) -> ProfileFile {
    ProfileFile {
        filename,
        destination: Some(destination),
        content_type: "application/json",
        content: serde_json::to_string_pretty(&content)
            .expect("JSON profile values always serialize"),
    }
}

fn pi_files(base_url: &str, model: &str, api_key: &str) -> Vec<ProfileFile> {
    let models = json!({
        "providers": {
            "kinetix": {
                "baseUrl": base_url,
                "apiKey": format!("${KEY_ENV}"),
                "api": "openai-completions",
                "models": [{ "id": model, "name": model }],
            }
        }
    });
    let settings = json!({
        "defaultProvider": "kinetix",
        "defaultModel": model,
    });
    vec![
        json_file("models.json", "~/.pi/agent/models.json", models),
        json_file("settings.json", "~/.pi/agent/settings.json", settings),
        api_key_file(api_key),
    ]
}

fn claude_code_file(root: &str, model: &str, api_key: &str) -> ProfileFile {
    let content = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\n\nexport ANTHROPIC_BASE_URL={}\nexport ANTHROPIC_API_KEY={}\nexport ANTHROPIC_AUTH_TOKEN=''\n\nexec claude --model {} \"$@\"\n",
        shell_quote(root),
        shell_quote(api_key),
        shell_quote(model),
    );
    ProfileFile {
        filename: "kinetix-claude.sh",
        destination: None,
        content_type: "text/x-shellscript",
        content,
    }
}

#[derive(Serialize)]
struct CodexConfig {
    model: String,
    model_provider: String,
    model_providers: BTreeMap<String, CodexProvider>,
}

#[derive(Serialize)]
struct CodexProvider {
    name: String,
    base_url: String,
    env_key: String,
    wire_api: String,
    requires_openai_auth: bool,
}

fn codex_files(base_url: &str, model: &str, api_key: &str) -> Vec<ProfileFile> {
    let config = CodexConfig {
        model: model.to_string(),
        model_provider: "kinetix".to_string(),
        model_providers: BTreeMap::from([(
            "kinetix".to_string(),
            CodexProvider {
                name: "Kinetix".to_string(),
                base_url: base_url.to_string(),
                env_key: KEY_ENV.to_string(),
                wire_api: "responses".to_string(),
                requires_openai_auth: false,
            },
        )]),
    };
    vec![
        ProfileFile {
            filename: "config.toml",
            destination: Some("~/.codex/config.toml"),
            content_type: "application/toml",
            content: toml::to_string_pretty(&config).expect("Codex config always serializes"),
        },
        api_key_file(api_key),
    ]
}

fn open_code_files(base_url: &str, model: &str, api_key: &str) -> Vec<ProfileFile> {
    let config = json!({
        "$schema": "https://opencode.ai/config.json",
        "model": "kinetix/default",
        "provider": {
            "kinetix": {
                "name": "Kinetix",
                "npm": "@ai-sdk/openai-compatible",
                "options": {
                    "baseURL": base_url,
                    "apiKey": format!("{{env:{KEY_ENV}}}"),
                },
                "models": {
                    "default": {
                        "id": model,
                        "name": model,
                    }
                },
            }
        }
    });
    vec![
        json_file("opencode.json", "opencode.json", config),
        api_key_file(api_key),
    ]
}

fn api_key_file(api_key: &str) -> ProfileFile {
    ProfileFile {
        filename: "kinetix-api-key.sh",
        destination: None,
        content_type: "text/x-shellscript",
        content: format!(
            "# Source this file in the shell that starts your client.\nexport {KEY_ENV}={}\n",
            shell_quote(api_key)
        ),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file<'a>(profile: &'a GeneratedProfile, filename: &str) -> &'a ProfileFile {
        profile
            .files
            .iter()
            .find(|file| file.filename == filename)
            .unwrap()
    }

    #[test]
    fn pi_profile_uses_chat_completions_and_persists_the_selected_default() {
        let profile = generate(
            ClientApp::Pi,
            "https://kinetix.example/gateway/v1/",
            "coder/route",
            Some("sk-kinetix-test"),
        );
        let models: Value = serde_json::from_str(&file(&profile, "models.json").content).unwrap();
        let settings: Value =
            serde_json::from_str(&file(&profile, "settings.json").content).unwrap();

        assert_eq!(
            models["providers"]["kinetix"]["baseUrl"],
            "https://kinetix.example/gateway/v1"
        );
        assert_eq!(models["providers"]["kinetix"]["api"], "openai-completions");
        assert_eq!(models["providers"]["kinetix"]["apiKey"], "$KINETIX_API_KEY");
        assert_eq!(
            models["providers"]["kinetix"]["models"][0]["id"],
            "coder/route"
        );
        assert_eq!(
            models["providers"]["kinetix"]["models"][0]["name"],
            "coder/route"
        );
        assert_eq!(settings["defaultProvider"], "kinetix");
        assert_eq!(settings["defaultModel"], "coder/route");
        assert!(file(&profile, "kinetix-api-key.sh")
            .content
            .contains("export KINETIX_API_KEY='sk-kinetix-test'"));
    }

    #[test]
    fn claude_code_receives_anthropic_root_and_model_without_openai_suffix() {
        let profile = generate(
            ClientApp::ClaudeCode,
            "https://kinetix.example/gateway/v1",
            "claude-compatible-route",
            Some("sk-kinetix-test"),
        );
        let script = &file(&profile, "kinetix-claude.sh").content;
        assert!(script.contains("export ANTHROPIC_BASE_URL='https://kinetix.example/gateway'"));
        assert!(script.contains("export ANTHROPIC_API_KEY='sk-kinetix-test'"));
        assert!(script.contains("export ANTHROPIC_AUTH_TOKEN=''"));
        assert!(script.contains("exec claude --model 'claude-compatible-route' \"$@\""));
        assert!(std::process::Command::new("bash")
            .args(["-n", "-c", script])
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn codex_profile_uses_the_responses_api_and_an_environment_key() {
        let profile = generate(
            ClientApp::Codex,
            "https://kinetix.example/",
            "coder",
            Some("sk-kinetix-test"),
        );
        let config: toml::Value = toml::from_str(&file(&profile, "config.toml").content).unwrap();

        assert_eq!(config["model"].as_str(), Some("coder"));
        assert_eq!(config["model_provider"].as_str(), Some("kinetix"));
        assert_eq!(
            config["model_providers"]["kinetix"]["base_url"].as_str(),
            Some("https://kinetix.example/v1")
        );
        assert_eq!(
            config["model_providers"]["kinetix"]["env_key"].as_str(),
            Some(KEY_ENV)
        );
        assert_eq!(
            config["model_providers"]["kinetix"]["wire_api"].as_str(),
            Some("responses")
        );
        assert_eq!(
            config["model_providers"]["kinetix"]["requires_openai_auth"].as_bool(),
            Some(false)
        );
        assert!(config["model_providers"]["kinetix"]
            .get("experimental_bearer_token")
            .is_none());
    }

    #[test]
    fn open_code_profile_uses_compatible_provider_without_guessing_model_metadata() {
        let profile = generate(
            ClientApp::OpenCode,
            "https://kinetix.example/v1/",
            "coder",
            None,
        );
        let config: Value = serde_json::from_str(&file(&profile, "opencode.json").content).unwrap();

        assert_eq!(config["model"], "kinetix/default");
        assert_eq!(
            config["provider"]["kinetix"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        assert_eq!(
            config["provider"]["kinetix"]["options"]["baseURL"],
            "https://kinetix.example/v1"
        );
        assert_eq!(
            config["provider"]["kinetix"]["options"]["apiKey"],
            format!("{{env:{KEY_ENV}}}")
        );
        assert_eq!(
            config["provider"]["kinetix"]["models"]["default"]["id"],
            "coder"
        );
        assert!(config["provider"]["kinetix"]["models"]["default"]["capabilities"].is_null());
        assert!(config["provider"]["kinetix"]["models"]["default"]["limit"].is_null());
        assert!(config["provider"]["kinetix"]["models"]["default"]["cost"].is_null());
        assert!(!file(&profile, "opencode.json")
            .content
            .contains("sk-kinetix-<paste-your-key>"));
        assert!(file(&profile, "kinetix-api-key.sh")
            .content
            .contains("sk-kinetix-<paste-your-key>"));
    }

    #[test]
    #[ignore = "run scripts/test-opencode-v1-profile.sh against stable OpenCode 1.18.33"]
    fn open_code_v1_profile_is_loaded_by_stable_cli() {
        const STABLE_VERSION: &str = "1.18.33";
        let executable = std::env::var_os("KINETIX_OPENCODE_V1_BIN")
            .expect("the compatibility script must provide the pinned OpenCode v1 CLI");
        let version = std::process::Command::new(&executable)
            .arg("--version")
            .output()
            .expect("OpenCode v1 CLI starts");
        assert!(version.status.success());
        assert_eq!(
            String::from_utf8_lossy(&version.stdout).trim(),
            STABLE_VERSION
        );

        let root = std::env::temp_dir().join(format!(
            "kinetix-opencode-v1-profile-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".config")).unwrap();
        std::fs::create_dir_all(home.join(".local/share")).unwrap();

        let profile = generate(
            ClientApp::OpenCode,
            "https://kinetix.example",
            "provider-b/model",
            None,
        );
        std::fs::write(
            root.join("opencode.json"),
            &file(&profile, "opencode.json").content,
        )
        .unwrap();
        let output = std::process::Command::new(executable)
            .args(["--log-level", "ERROR", "debug", "config"])
            .current_dir(&root)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env(KEY_ENV, "test-only-virtual-key")
            .output()
            .expect("OpenCode v1 debug config starts");
        assert!(
            output.status.success(),
            "OpenCode rejected the generated config: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let resolved: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(resolved["model"], "kinetix/default");
        assert_eq!(
            resolved["provider"]["kinetix"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        assert_eq!(
            resolved["provider"]["kinetix"]["options"]["baseURL"],
            "https://kinetix.example/v1"
        );
        assert_eq!(
            resolved["provider"]["kinetix"]["models"]["default"]["id"],
            "provider-b/model"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn public_url_suffix_is_applied_only_to_openai_style_clients() {
        let pi = generate(ClientApp::Pi, "https://kinetix.example", "m", None);
        let claude = generate(ClientApp::ClaudeCode, "https://kinetix.example", "m", None);
        let codex = generate(ClientApp::Codex, "https://kinetix.example", "m", None);
        let opencode = generate(ClientApp::OpenCode, "https://kinetix.example", "m", None);
        let pi_json: Value = serde_json::from_str(&file(&pi, "models.json").content).unwrap();
        let codex_toml: toml::Value = toml::from_str(&file(&codex, "config.toml").content).unwrap();
        let opencode_json: Value =
            serde_json::from_str(&file(&opencode, "opencode.json").content).unwrap();

        assert_eq!(
            pi_json["providers"]["kinetix"]["baseUrl"],
            "https://kinetix.example/v1"
        );
        assert_eq!(
            codex_toml["model_providers"]["kinetix"]["base_url"].as_str(),
            Some("https://kinetix.example/v1")
        );
        assert_eq!(
            opencode_json["provider"]["kinetix"]["options"]["baseURL"],
            "https://kinetix.example/v1"
        );
        assert!(file(&claude, "kinetix-claude.sh")
            .content
            .contains("export ANTHROPIC_BASE_URL='https://kinetix.example'"));
    }
}
