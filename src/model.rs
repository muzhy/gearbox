use std::{collections::HashSet, time::Duration};

use anyhow::{Result, bail, ensure};
use reqwest::{
    Client, Url,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde_json::{Value, json};

use crate::config::file::{Limits, ModelConfig, ReasoningEffort};

pub struct ModelClient {
    client: Client,
    endpoint: Url,
    model: String,
    reasoning_effort: ReasoningEffort,
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

impl ModelClient {
    pub fn new(config: &ModelConfig, limits: &Limits) -> Result<Self> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", config.api_key))
            .expect("configuration validates the authentication header");
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(limits.request_timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| anyhow::anyhow!("无法初始化模型 HTTP 客户端"))?;
        Ok(Self {
            client,
            endpoint: config.endpoint.clone(),
            model: config.name.clone(),
            reasoning_effort: config.reasoning_effort,
            max_output_tokens: limits.max_output_tokens,
        })
    }

    pub async fn respond(&self, history: &[Value], tool_definition: Value) -> Result<ModelTurn> {
        let request = json!({
            "model": self.model,
            "input": history,
            "tools": [tool_definition],
            "stream": false,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "reasoning": { "effort": self.reasoning_effort },
            "max_output_tokens": self.max_output_tokens,
            "truncation": "disabled",
        });
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

// reqwest errors can contain the request URL; never propagate them or raw server bodies.
fn transport_error(error: reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow::anyhow!("模型请求超时")
    } else {
        anyhow::anyhow!("模型请求网络或传输失败")
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
}
