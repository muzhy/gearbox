use std::{collections::HashSet, time::Duration};

use anyhow::{Result, bail, ensure};
use reqwest::{
    Client, Url,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::file::{
    DeprecatedModelPolicy, DiscoveryErrorPolicy, Limits, MissingModelPolicy, ModelConfig,
    ModelDiscoveryConfig, ProviderConfig,
};

pub struct ModelClient {
    client: Client,
    endpoint: Url,
    model: String,
    reasoning: bool,
    max_output_tokens: u32,
}

pub struct ModelTurn {
    pub output: Vec<Value>,
    pub calls: Vec<ToolCall>,
    pub text: String,
}

pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    id: String,
    #[serde(default)]
    deprecated: bool,
    status: Option<String>,
}

impl ModelClient {
    pub fn new(config: &ModelConfig, provider: &ProviderConfig, limits: &Limits) -> Result<Self> {
        Ok(Self {
            client: provider_client(provider, limits)?,
            endpoint: provider.responses_endpoint(),
            model: config.id.clone(),
            reasoning: config.reasoning,
            max_output_tokens: limits.max_output_tokens,
        })
    }

    pub async fn respond(&self, history: &[Value], tool_definition: Value) -> Result<ModelTurn> {
        let mut request = json!({
            "model": self.model,
            "input": history,
            "tools": [tool_definition],
            "stream": false,
            "store": false,
            "max_output_tokens": self.max_output_tokens,
            "truncation": "disabled",
        });
        if self.reasoning {
            request["include"] = json!(["reasoning.encrypted_content"]);
            // Let the service choose its default reasoning effort. The model
            // capability is configuration; an effort value is not a model ID.
            request["reasoning"] = json!({});
        }
        let response = self
            .client
            .post(self.endpoint.clone())
            .json(&request)
            .send()
            .await
            .map_err(transport_error)?;
        ensure!(
            response.status().is_success(),
            "模型服务返回 HTTP {}",
            response.status().as_u16()
        );
        let bytes = response.bytes().await.map_err(transport_error)?;
        let response: Value =
            serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("模型响应不是有效 JSON"))?;
        parse_response(response)
    }
}

pub(crate) async fn validate_configured_model(
    config: &ModelConfig,
    provider: &ProviderConfig,
    limits: &Limits,
) -> Result<()> {
    let Some(discovery) = &provider.discovery else {
        return Ok(());
    };
    let catalog = fetch_model_catalog(provider, &discovery.path, limits).await;
    match catalog {
        Ok(catalog) => reconcile_model_catalog(&config.id, discovery, &catalog),
        Err(error) => match discovery.on_error {
            DiscoveryErrorPolicy::UseConfig => {
                tracing::warn!(
                    provider = %config.provider,
                    model = %config.id,
                    error = %error,
                    "model discovery failed; using the configured model"
                );
                Ok(())
            }
            DiscoveryErrorPolicy::Error => Err(error),
        },
    }
}

fn provider_client(provider: &ProviderConfig, limits: &Limits) -> Result<Client> {
    let mut authorization = HeaderValue::from_str(&format!("Bearer {}", provider.api_key))
        .expect("configuration validates the authentication header");
    authorization.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, authorization);
    Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(limits.request_timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| anyhow::anyhow!("无法初始化模型 HTTP 客户端"))
}

async fn fetch_model_catalog(
    provider: &ProviderConfig,
    path: &str,
    limits: &Limits,
) -> Result<Vec<CatalogModel>> {
    let response = provider_client(provider, limits)?
        .get(provider.endpoint(path))
        .send()
        .await
        .map_err(catalog_transport_error)?;
    ensure!(
        response.status().is_success(),
        "模型目录服务返回 HTTP {}",
        response.status().as_u16()
    );
    let bytes = response.bytes().await.map_err(catalog_transport_error)?;
    let catalog: ModelList =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("模型目录响应格式无效"))?;
    ensure!(
        catalog.models_have_valid_ids(),
        "模型目录包含无效的模型标识"
    );
    Ok(catalog.data)
}

pub(crate) async fn fetch_provider_models(
    provider: &ProviderConfig,
    path: &str,
    limits: &Limits,
) -> Result<Vec<String>> {
    Ok(fetch_model_catalog(provider, path, limits)
        .await?
        .into_iter()
        .map(|model| model.id)
        .collect())
}

impl ModelList {
    fn models_have_valid_ids(&self) -> bool {
        self.data.iter().all(|model| !model.id.trim().is_empty())
    }
}

fn reconcile_model_catalog(
    configured_id: &str,
    discovery: &ModelDiscoveryConfig,
    catalog: &[CatalogModel],
) -> Result<()> {
    let Some(model) = catalog.iter().find(|model| model.id == configured_id) else {
        return match discovery.on_missing {
            MissingModelPolicy::Allow => Ok(()),
            MissingModelPolicy::Warn => {
                tracing::warn!(
                    model = configured_id,
                    "configured model is absent from the provider catalog; using it unchanged"
                );
                Ok(())
            }
            MissingModelPolicy::Error => {
                bail!("configured model is absent from the provider catalog")
            }
        };
    };

    let deprecated = model.deprecated
        || model
            .status
            .as_deref()
            .is_some_and(|status| status.eq_ignore_ascii_case("deprecated"));
    if !deprecated {
        return Ok(());
    }
    match discovery.on_deprecated {
        DeprecatedModelPolicy::Ignore => Ok(()),
        DeprecatedModelPolicy::Warn => {
            tracing::warn!(
                model = configured_id,
                "configured model is deprecated; using it unchanged"
            );
            Ok(())
        }
        DeprecatedModelPolicy::Error => bail!("configured model is deprecated"),
    }
}

// reqwest errors can contain the request URL; never propagate them or raw server bodies.
fn transport_error(error: reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow::anyhow!("模型请求超时")
    } else {
        anyhow::anyhow!("模型请求网络或传输失败")
    }
}

fn catalog_transport_error(error: reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow::anyhow!("模型目录请求超时")
    } else {
        anyhow::anyhow!("模型目录请求网络或传输失败")
    }
}

fn nonempty_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    let text = value[field]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("模型响应缺少有效的 {field} 字符串字段"))?;
    ensure!(!text.trim().is_empty(), "模型响应的 {field} 字段不能为空");
    Ok(text)
}

fn optional_completed_status(item: &Value) -> Result<()> {
    if let Some(status) = item.get("status") {
        ensure!(status.as_str() == Some("completed"), "模型输出项未正常完成");
    }
    Ok(())
}

fn parse_response(response: Value) -> Result<ModelTurn> {
    ensure!(response.is_object(), "模型响应必须是 JSON 对象");
    ensure!(response["error"].is_null(), "模型服务报告响应错误");
    ensure!(
        response["incomplete_details"].is_null(),
        "模型响应不完整或已截断"
    );
    ensure!(
        response["status"].as_str() == Some("completed"),
        "模型响应未正常完成"
    );
    let output = response["output"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("模型响应缺少 output 数组"))?;
    ensure!(!output.is_empty(), "模型返回空响应");

    let mut calls = Vec::new();
    let mut call_ids = HashSet::new();
    let mut text = Vec::new();
    for item in output {
        ensure!(item.is_object(), "模型输出项必须是 JSON 对象");
        match item["type"].as_str() {
            Some("message") => {
                nonempty_string(item, "id")?;
                ensure!(
                    item["role"].as_str() == Some("assistant"),
                    "模型消息 role 必须是 assistant"
                );
                ensure!(
                    item["status"].as_str() == Some("completed"),
                    "模型消息未正常完成"
                );
                let content = item["content"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("模型消息缺少 content 数组"))?;
                for part in content {
                    match part["type"].as_str() {
                        Some("output_text") => {
                            let part_text = part["text"]
                                .as_str()
                                .ok_or_else(|| anyhow::anyhow!("模型文本内容缺少 text 字符串"))?;
                            text.push(part_text);
                        }
                        Some("refusal") => bail!("模型拒绝回答本次任务"),
                        _ => bail!("模型消息包含不支持的内容类型"),
                    }
                }
            }
            Some("function_call") => {
                optional_completed_status(item)?;
                if item.get("id").is_some() {
                    nonempty_string(item, "id")?;
                }
                let call_id = nonempty_string(item, "call_id")?;
                ensure!(call_ids.insert(call_id), "模型响应中的 call_id 重复");
                let name = nonempty_string(item, "name")?;
                let arguments = item["arguments"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("模型函数调用缺少 arguments 字符串"))?;
                calls.push(ToolCall {
                    call_id: call_id.to_owned(),
                    name: name.to_owned(),
                    arguments: arguments.to_owned(),
                });
            }
            Some("reasoning") => {
                nonempty_string(item, "id")?;
                if !item["status"].is_null() {
                    optional_completed_status(item)?;
                }
                let summary = item["summary"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("模型推理项缺少 summary 数组"))?;
                for part in summary {
                    ensure!(
                        part["type"].as_str() == Some("summary_text") && part["text"].is_string(),
                        "模型推理摘要结构无效"
                    );
                }
                if let Some(content) = item.get("content").filter(|value| !value.is_null()) {
                    let content = content
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("模型推理 content 必须是数组"))?;
                    for part in content {
                        ensure!(
                            part["type"].as_str() == Some("reasoning_text")
                                && part["text"].is_string(),
                            "模型推理内容结构无效"
                        );
                    }
                }
                if let Some(encrypted) = item.get("encrypted_content") {
                    ensure!(
                        encrypted.is_null() || encrypted.is_string(),
                        "模型加密推理内容必须是字符串"
                    );
                }
            }
            _ => bail!("模型返回不支持的输出类型"),
        }
    }

    let text = text.join("\n");
    ensure!(
        !calls.is_empty() || !text.trim().is_empty(),
        "模型响应没有工具调用或非空回答"
    );
    Ok(ModelTurn {
        output: output.clone(),
        calls,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discovery(
        on_missing: MissingModelPolicy,
        on_deprecated: DeprecatedModelPolicy,
    ) -> ModelDiscoveryConfig {
        ModelDiscoveryConfig {
            path: "models".to_owned(),
            on_missing,
            on_deprecated,
            on_error: DiscoveryErrorPolicy::UseConfig,
        }
    }

    fn catalog_model(id: &str, deprecated: bool, status: Option<&str>) -> CatalogModel {
        CatalogModel {
            id: id.to_owned(),
            deprecated,
            status: status.map(str::to_owned),
        }
    }

    fn message(text: &str) -> Value {
        json!({
            "type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
            "phase": "final_answer",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
        })
    }

    fn function_call(id: &str) -> Value {
        json!({
            "type": "function_call", "id": format!("fc_{id}"), "call_id": id,
            "name": "read_file", "arguments": "{\"path\":\"index.txt\"}",
            "status": "completed",
        })
    }

    fn completed(output: Vec<Value>) -> Value {
        json!({ "status": "completed", "error": null, "incomplete_details": null, "output": output })
    }

    #[test]
    fn preserves_all_items_including_encrypted_reasoning_and_message_phase() {
        let output = vec![
            json!({
                "type": "reasoning", "id": "rs_1", "summary": [],
                "encrypted_content": "opaque_encrypted_reasoning", "provider_extension": 42,
            }),
            message("我先读取索引。"),
            function_call("call_1"),
            function_call("call_2"),
        ];
        let turn = parse_response(completed(output.clone())).unwrap();
        assert_eq!(turn.output, output);
        assert_eq!(turn.text, "我先读取索引。");
        assert_eq!(turn.calls.len(), 2);
        assert_eq!(turn.calls[0].call_id, "call_1");
        assert_eq!(turn.calls[1].call_id, "call_2");
    }

    #[test]
    fn allows_tool_to_handle_unknown_names_and_invalid_argument_json() {
        let mut call = function_call("call_1");
        call["name"] = json!("unknown_tool");
        call["arguments"] = json!("not JSON");
        call.as_object_mut().unwrap().remove("status");
        call.as_object_mut().unwrap().remove("id");
        let turn = parse_response(completed(vec![call])).unwrap();
        assert_eq!(turn.calls[0].name, "unknown_tool");
        assert_eq!(turn.calls[0].arguments, "not JSON");
    }

    #[test]
    fn rejects_missing_empty_or_duplicate_call_ids_for_entire_batch() {
        for bad_id in [Value::Null, json!(""), json!("   "), json!(12)] {
            let mut call = function_call("call_2");
            call["call_id"] = bad_id;
            assert!(parse_response(completed(vec![function_call("call_1"), call])).is_err());
        }
        let mut missing = function_call("call_2");
        missing.as_object_mut().unwrap().remove("call_id");
        assert!(parse_response(completed(vec![function_call("call_1"), missing])).is_err());
        assert!(
            parse_response(completed(vec![
                function_call("call_1"),
                function_call("call_1")
            ]))
            .is_err()
        );
    }

    #[test]
    fn rejects_failed_incomplete_refused_or_empty_responses() {
        let good = completed(vec![message("answer")]);
        for status in ["in_progress", "incomplete", "failed", "cancelled", "queued"] {
            let mut response = good.clone();
            response["status"] = json!(status);
            assert!(parse_response(response).is_err());
        }
        for field in ["error", "incomplete_details"] {
            let mut response = good.clone();
            response[field] = json!({ "message": "sensitive server body" });
            let error = parse_response(response).err().unwrap().to_string();
            assert!(!error.contains("sensitive server body"));
        }
        let mut refusal = message("");
        refusal["content"] = json!([{ "type": "refusal", "refusal": "cannot comply" }]);
        assert!(parse_response(completed(vec![function_call("call_1"), refusal])).is_err());
        assert!(parse_response(completed(vec![])).is_err());
        assert!(parse_response(completed(vec![message(" \n")])).is_err());
        assert!(
            parse_response(completed(vec![json!({
                "type": "reasoning", "id": "rs_1", "summary": [],
            })]))
            .is_err()
        );
    }

    #[test]
    fn rejects_malformed_or_unsupported_output() {
        let malformed = vec![
            Value::Null,
            json!({ "type": "web_search_call" }),
            json!({ "type": "message", "role": "assistant", "content": [] }),
            json!({ "type": "function_call", "call_id": "c", "name": "read_file", "arguments": {} }),
            json!({ "type": "reasoning", "id": "r", "summary": "not an array" }),
            json!({ "type": "reasoning", "id": "r", "summary": [], "encrypted_content": 42 }),
        ];
        for item in malformed {
            assert!(parse_response(completed(vec![function_call("valid"), item])).is_err());
        }
        for field in ["id", "role", "status", "content"] {
            let mut item = message("answer");
            item.as_object_mut().unwrap().remove(field);
            assert!(parse_response(completed(vec![item])).is_err());
        }
        let mut item = message("answer");
        item["status"] = json!("incomplete");
        assert!(parse_response(completed(vec![item])).is_err());
        let mut item = message("answer");
        item["content"][0]["type"] = json!("unsupported");
        assert!(parse_response(completed(vec![item])).is_err());
    }

    #[test]
    fn accepts_a_direct_answer() {
        let turn = parse_response(completed(vec![message("Gearbox Demo")])).unwrap();
        assert!(turn.calls.is_empty());
        assert_eq!(turn.text, "Gearbox Demo");
    }

    #[test]
    fn catalog_conflicts_follow_the_configured_policies() {
        let available = [catalog_model("configured", false, None)];
        assert!(
            reconcile_model_catalog(
                "configured",
                &discovery(MissingModelPolicy::Error, DeprecatedModelPolicy::Error),
                &available,
            )
            .is_ok()
        );

        let empty = [];
        assert!(
            reconcile_model_catalog(
                "configured",
                &discovery(MissingModelPolicy::Allow, DeprecatedModelPolicy::Warn),
                &empty,
            )
            .is_ok()
        );
        assert!(
            reconcile_model_catalog(
                "configured",
                &discovery(MissingModelPolicy::Warn, DeprecatedModelPolicy::Warn),
                &empty,
            )
            .is_ok()
        );
        assert!(
            reconcile_model_catalog(
                "configured",
                &discovery(MissingModelPolicy::Error, DeprecatedModelPolicy::Warn),
                &empty,
            )
            .is_err()
        );

        for deprecated in [
            catalog_model("configured", true, None),
            catalog_model("configured", false, Some("deprecated")),
        ] {
            assert!(
                reconcile_model_catalog(
                    "configured",
                    &discovery(MissingModelPolicy::Warn, DeprecatedModelPolicy::Ignore),
                    std::slice::from_ref(&deprecated),
                )
                .is_ok()
            );
            assert!(
                reconcile_model_catalog(
                    "configured",
                    &discovery(MissingModelPolicy::Warn, DeprecatedModelPolicy::Warn),
                    std::slice::from_ref(&deprecated),
                )
                .is_ok()
            );
            assert!(
                reconcile_model_catalog(
                    "configured",
                    &discovery(MissingModelPolicy::Warn, DeprecatedModelPolicy::Error),
                    std::slice::from_ref(&deprecated),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn parses_openai_compatible_catalog_metadata() {
        let catalog: ModelList = serde_json::from_value(json!({
            "object": "list",
            "data": [
                {"id": "active", "object": "model", "owned_by": "provider"},
                {"id": "old", "deprecated": true},
                {"id": "legacy", "status": "deprecated"}
            ]
        }))
        .unwrap();
        assert!(catalog.models_have_valid_ids());
        assert_eq!(catalog.data.len(), 3);
        assert!(!catalog.data[0].deprecated);
        assert!(catalog.data[1].deprecated);
        assert_eq!(catalog.data[2].status.as_deref(), Some("deprecated"));

        let invalid: ModelList = serde_json::from_value(json!({"data": [{"id": " "}]})).unwrap();
        assert!(!invalid.models_have_valid_ids());
        assert!(serde_json::from_value::<ModelList>(json!({"data": [{"id": 1}]})).is_err());
    }
}
