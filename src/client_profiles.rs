//! Generate client connection files from Kinetix's public endpoint and a
//! client-visible model or Route. These files configure clients to call only
//! Kinetix; upstream provider credentials are never part of the profile.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{json, Value};

const KEY_ENV: &str = "KINETIX_API_KEY";
const KEY_PLACEHOLDER: &str = "sk-kinetix-<paste-your-key>";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ClientApp {
    Pi,
    ClaudeCode,
    Codex,
    OpenCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileFileUsage {
    WriteTo,
    MergeInto,
    Source,
    Execute,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileFile {
    pub filename: &'static str,
    pub destination: Option<&'static str>,
    pub usage: ProfileFileUsage,
    pub content_type: &'static str,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ClientModelMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
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
    generate_with_metadata(
        client,
        public_base_url,
        model,
        &ClientModelMetadata::default(),
        api_key,
    )
}

pub fn generate_with_metadata(
    client: ClientApp,
    public_base_url: &str,
    model: &str,
    metadata: &ClientModelMetadata,
    api_key: Option<&str>,
) -> GeneratedProfile {
    let root = endpoint_root(public_base_url);
    let openai_base_url = format!("{root}/v1");
    let api_key = api_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .unwrap_or(KEY_PLACEHOLDER);

    let files = match client {
        ClientApp::Pi => pi_files(&openai_base_url, model, metadata, api_key),
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
        usage: ProfileFileUsage::MergeInto,
        content_type: "application/json",
        content: serde_json::to_string_pretty(&content)
            .expect("JSON profile values always serialize"),
    }
}

fn pi_files(
    base_url: &str,
    model: &str,
    metadata: &ClientModelMetadata,
    api_key: &str,
) -> Vec<ProfileFile> {
    let mut model_config = json!({
        "id": model,
        "name": model,
        // Match the Pi session-affinity configuration exercised by the real
        // client acceptance fixture and documented in docs/pi-compatibility.md.
        "compat": {
            "sendSessionAffinityHeaders": true,
            "sessionAffinityFormat": "openrouter"
        }
    });
    if let Some(reasoning) = metadata.reasoning {
        model_config["reasoning"] = json!(reasoning);
    }
    if let Some(input) = &metadata.input {
        model_config["input"] = json!(input);
    }
    if let Some(context_window) = metadata.context_window {
        model_config["contextWindow"] = json!(context_window);
    }
    if let Some(max_output_tokens) = metadata.max_output_tokens {
        model_config["maxTokens"] = json!(max_output_tokens);
    }
    let models = json!({
        "providers": {
            "kinetix": {
                "baseUrl": base_url,
                "apiKey": format!("${KEY_ENV}"),
                "api": "openai-completions",
                "models": [model_config],
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
        usage: ProfileFileUsage::Execute,
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
            usage: ProfileFileUsage::MergeInto,
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
        usage: ProfileFileUsage::Source,
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
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::{Duration, Instant};

    fn file<'a>(profile: &'a GeneratedProfile, filename: &str) -> &'a ProfileFile {
        profile
            .files
            .iter()
            .find(|file| file.filename == filename)
            .unwrap()
    }

    #[derive(Clone)]
    struct CapturedRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: Value,
    }

    fn read_stub_request(stream: &mut TcpStream) -> std::io::Result<CapturedRequest> {
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        let (header_end, content_length) = loop {
            let count = stream.read(&mut chunk)?;
            if count == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "client closed before completing the request",
                ));
            }
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.trim().parse::<usize>())
                    .transpose()
                    .map_err(std::io::Error::other)?
                    .unwrap_or(0);
                if bytes.len() >= header_end + 4 + content_length {
                    break (header_end, content_length);
                }
            }
        };
        let headers_text = String::from_utf8_lossy(&bytes[..header_end]);
        let mut lines = headers_text.lines();
        let mut request_line = lines.next().unwrap_or_default().split_whitespace();
        let method = request_line.next().unwrap_or_default().to_string();
        let path = request_line.next().unwrap_or_default().to_string();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        let body_start = header_end + 4;
        let body = serde_json::from_slice(&bytes[body_start..body_start + content_length])
            .unwrap_or(Value::Null);
        Ok(CapturedRequest {
            method,
            path,
            headers,
            body,
        })
    }

    fn spawn_opencode_stub(listener: TcpListener) -> thread::JoinHandle<Vec<CapturedRequest>> {
        thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut last_request = Instant::now();
            let mut requests = Vec::new();
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let request = read_stub_request(&mut stream).unwrap();
                        let is_completion =
                            request.method == "POST" && request.path == "/v1/chat/completions";
                        let (status, content_type, body) = if is_completion {
                            let chunks = [
                                json!({
                                    "id": "chatcmpl-stub",
                                    "object": "chat.completion.chunk",
                                    "created": 0,
                                    "model": "provider-b/model",
                                    "choices": [{
                                        "index": 0,
                                        "delta": {"role": "assistant", "content": "stub response"},
                                        "finish_reason": null
                                    }]
                                }),
                                json!({
                                    "id": "chatcmpl-stub",
                                    "object": "chat.completion.chunk",
                                    "created": 0,
                                    "model": "provider-b/model",
                                    "choices": [{
                                        "index": 0,
                                        "delta": {},
                                        "finish_reason": "stop"
                                    }]
                                }),
                            ];
                            let events = chunks
                                .iter()
                                .map(|chunk| format!("data: {chunk}\r\n\r\n"))
                                .collect::<String>()
                                + "data: [DONE]\r\n\r\n";
                            ("200 OK", "text/event-stream", events)
                        } else if request.method == "GET" && request.path.ends_with("/models") {
                            (
                                "200 OK",
                                "application/json",
                                json!({"data": [{"id": "provider-b/model"}]}).to_string(),
                            )
                        } else {
                            ("404 Not Found", "application/json", "{}".to_string())
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream.write_all(response.as_bytes()).unwrap();
                        stream.flush().unwrap();
                        requests.push(request);
                        if is_completion {
                            last_request = Instant::now();
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if !requests.is_empty() && last_request.elapsed() > Duration::from_secs(2) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("OpenCode stub listener failed: {error}"),
                }
            }
            requests
        })
    }

    #[test]
    fn profile_file_usage_serializes_for_the_dashboard() {
        for (usage, expected) in [
            (ProfileFileUsage::WriteTo, "write_to"),
            (ProfileFileUsage::MergeInto, "merge_into"),
            (ProfileFileUsage::Source, "source"),
            (ProfileFileUsage::Execute, "execute"),
        ] {
            assert_eq!(serde_json::to_value(usage).unwrap(), expected);
        }
    }

    #[test]
    fn pi_profile_omits_unknown_model_metadata_but_enables_session_affinity() {
        let profile = generate(
            ClientApp::Pi,
            "https://kinetix.example",
            "coder/model",
            None,
        );
        let models: Value = serde_json::from_str(&file(&profile, "models.json").content).unwrap();
        let model = &models["providers"]["kinetix"]["models"][0];
        for field in ["reasoning", "input", "contextWindow", "maxTokens"] {
            assert!(
                model.get(field).is_none(),
                "unexpected model metadata: {field}"
            );
        }
        assert_eq!(
            model["compat"],
            json!({
                "sendSessionAffinityHeaders": true,
                "sessionAffinityFormat": "openrouter"
            })
        );
    }

    #[test]
    fn pi_profile_uses_chat_completions_and_persists_the_selected_default() {
        let metadata = ClientModelMetadata {
            reasoning: Some(true),
            input: Some(vec!["text".into(), "image".into()]),
            context_window: Some(200_000),
            max_output_tokens: Some(8_192),
        };
        let profile = generate_with_metadata(
            ClientApp::Pi,
            "https://kinetix.example/gateway/v1/",
            "coder/route",
            &metadata,
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
        assert_eq!(
            models["providers"]["kinetix"]["models"][0]["reasoning"],
            true
        );
        assert_eq!(
            models["providers"]["kinetix"]["models"][0]["input"],
            json!(["text", "image"])
        );
        assert_eq!(
            models["providers"]["kinetix"]["models"][0]["contextWindow"],
            200_000
        );
        assert_eq!(
            models["providers"]["kinetix"]["models"][0]["maxTokens"],
            8_192
        );
        assert_eq!(
            models["providers"]["kinetix"]["models"][0]["compat"],
            json!({
                "sendSessionAffinityHeaders": true,
                "sessionAffinityFormat": "openrouter"
            })
        );
        assert_eq!(settings["defaultProvider"], "kinetix");
        assert_eq!(settings["defaultModel"], "coder/route");
        assert_eq!(
            file(&profile, "settings.json").usage,
            ProfileFileUsage::MergeInto
        );
        assert_eq!(
            file(&profile, "kinetix-api-key.sh").usage,
            ProfileFileUsage::Source
        );
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
        let helper = file(&profile, "kinetix-claude.sh");
        assert_eq!(helper.usage, ProfileFileUsage::Execute);
        let script = &helper.content;
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
        let config_file = file(&profile, "config.toml");
        assert_eq!(config_file.usage, ProfileFileUsage::MergeInto);
        assert_eq!(
            file(&profile, "kinetix-api-key.sh").usage,
            ProfileFileUsage::Source
        );
        let config: toml::Value = toml::from_str(&config_file.content).unwrap();

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
        let config_file = file(&profile, "opencode.json");
        assert_eq!(config_file.usage, ProfileFileUsage::MergeInto);
        let config: Value = serde_json::from_str(&config_file.content).unwrap();

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
    fn open_code_v1_profile_sends_selected_model_to_kinetix_stub() {
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let stub = spawn_opencode_stub(listener);

        let profile = generate(ClientApp::OpenCode, &base_url, "provider-b/model", None);
        std::fs::write(
            root.join("opencode.json"),
            &file(&profile, "opencode.json").content,
        )
        .unwrap();
        let output = std::process::Command::new(&executable)
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
            base_url
        );
        assert_eq!(
            resolved["provider"]["kinetix"]["models"]["default"]["id"],
            "provider-b/model"
        );

        let output = std::process::Command::new(&executable)
            .args([
                "--print-logs",
                "--log-level",
                "DEBUG",
                "run",
                "--model",
                "kinetix/default",
                "--format",
                "json",
                "Reply with the stub response.",
            ])
            .current_dir(&root)
            .env("PWD", &root)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env(KEY_ENV, "test-only-virtual-key")
            .output()
            .expect("OpenCode v1 run starts");
        let requests = stub.join().expect("OpenCode stub completes");
        assert!(
            output.status.success(),
            "OpenCode request failed: {} request(s) captured; stdout={} stderr={}",
            requests.len(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let request = requests
            .iter()
            .find(|request| request.method == "POST" && request.path == "/v1/chat/completions")
            .expect("OpenCode must POST a completion request to Kinetix");
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/chat/completions");
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer test-only-virtual-key")
        );
        assert_eq!(request.body["model"], "provider-b/model");
        assert_eq!(request.body["stream"], true);

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
