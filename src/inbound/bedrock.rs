use super::*;
pub(super) fn request(
    uri: &Uri,
    body: &Value,
    format: Format,
) -> Result<(Value, String, &'static str), TranslateError> {
    let resource = uri
        .path()
        .strip_prefix("/model/")
        .ok_or_else(|| invalid("Unknown Bedrock path"))?;
    let (model, op) = resource
        .rsplit_once('/')
        .ok_or_else(|| invalid("A model and operation are required"))?;
    // Decode the SDK's encoded modelId once, without treating it as a URL.
    let model = super::resource(model)?;
    if model.is_empty() || model.chars().any(char::is_control) {
        return Err(invalid("Invalid model ID"));
    }
    let stream = matches!(op, "converse-stream" | "invoke-with-response-stream");
    if format == Format::Invoke {
        if !matches!(op, "invoke" | "invoke-with-response-stream")
            || body["anthropic_version"] != "bedrock-2023-05-31"
        {
            return Err(invalid(
                "Invoke supports only anthropic_version bedrock-2023-05-31",
            ));
        }
        let mut out = body.clone();
        let obj = out
            .as_object_mut()
            .ok_or_else(|| invalid("The request must be an object"))?;
        obj.remove("anthropic_version");
        obj.insert("model".into(), json!(model));
        obj.insert("stream".into(), json!(stream));
        return Ok((out, op.into(), "/v1/messages"));
    }
    if !matches!(op, "converse" | "converse-stream") {
        return Err(invalid("Unsupported Bedrock operation"));
    }
    for key in [
        "guardrailConfig",
        "additionalModelRequestFields",
        "additionalModelResponseFieldPaths",
        "promptVariables",
        "performanceConfig",
    ] {
        if body.get(key).is_some() {
            return Err(invalid(&format!("{key} cannot be translated")));
        }
    }
    let mut messages = Vec::new();
    if let Some(system) = body.get("system") {
        let mut text = String::new();
        for p in array(system) {
            text.push_str(
                p["text"]
                    .as_str()
                    .ok_or_else(|| invalid("system must contain text only"))?,
            );
        }
        messages.push(json!({"role":"system","content":text}));
    }
    for message in body["messages"]
        .as_array()
        .ok_or_else(|| invalid("messages must be an array"))?
    {
        let role = message["role"]
            .as_str()
            .filter(|r| matches!(*r, "user" | "assistant"))
            .ok_or_else(|| invalid("Unsupported Converse role"))?;
        let mut parts = Vec::new();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for p in message["content"]
            .as_array()
            .ok_or_else(|| invalid("content must be an array"))?
        {
            if let Some(text) = p.get("text") {
                parts.push(json!({"type":"text","text":text}));
            } else if let Some(image) = p.get("image") {
                let format = image["format"]
                    .as_str()
                    .ok_or_else(|| invalid("Image format is required"))?;
                let bytes = image["source"]["bytes"]
                    .as_str()
                    .ok_or_else(|| invalid("Image source.bytes is required"))?;
                parts.push(json!({"type":"image_url","image_url":{"url":format!("data:image/{format};base64,{bytes}")}}));
            } else if let Some(tool) = p.get("toolUse") {
                if !tool["toolUseId"].is_string()
                    || !tool["name"].is_string()
                    || !tool["input"].is_object()
                {
                    return Err(invalid("Invalid toolUse"));
                }
                calls.push(json!({"id":tool["toolUseId"],"type":"function","function":{"name":tool["name"],"arguments":tool["input"].to_string()}}));
            } else if let Some(tool) = p.get("toolResult") {
                let mut text = String::new();
                for p in array(&tool["content"]) {
                    if let Some(t) = p["text"].as_str() {
                        text.push_str(t);
                    } else if let Some(v) = p.get("json") {
                        text.push_str(&v.to_string());
                    } else {
                        return Err(invalid(
                            "Only text and JSON toolResult content can be translated",
                        ));
                    }
                }
                results
                    .push(json!({"role":"tool","tool_call_id":tool["toolUseId"],"content":text}));
            } else {
                return Err(invalid(
                    "Unsupported Converse content; use text, image, toolUse or toolResult",
                ));
            }
        }
        messages.extend(results);
        if !parts.is_empty() || !calls.is_empty() {
            let mut m =
                json!({"role":role,"content":if parts.is_empty(){Value::Null}else{json!(parts)}});
            if !calls.is_empty() {
                m["tool_calls"] = json!(calls);
            }
            messages.push(m);
        }
    }
    let mut out = json!({"model":model,"messages":messages,"stream":stream});
    if stream {
        out["stream_options"] = json!({"include_usage":true});
    }
    for (from, to) in [
        ("maxTokens", "max_tokens"),
        ("temperature", "temperature"),
        ("topP", "top_p"),
        ("stopSequences", "stop"),
    ] {
        if let Some(v) = body["inferenceConfig"].get(from) {
            out[to] = v.clone();
        }
    }
    if let Some(config) = body.get("toolConfig") {
        let tools = array(&config["tools"])
            .iter()
            .map(|t| {
                let t = &t["toolSpec"];
                if !t["name"].is_string() || !t["inputSchema"]["json"].is_object() {
                    return Err(invalid("Only JSON toolSpec tools can be translated"));
                }
                let mut f = json!({"name":t["name"],"parameters":t["inputSchema"]["json"]});
                if let Some(d) = t.get("description") {
                    f["description"] = d.clone();
                }
                Ok(json!({"type":"function","function":f}))
            })
            .collect::<Result<Vec<_>, _>>()?;
        out["tools"] = json!(tools);
        if let Some(choice) = config.get("toolChoice") {
            out["tool_choice"] = if choice.get("auto").is_some() {
                json!("auto")
            } else if choice.get("any").is_some() {
                json!("required")
            } else if choice["tool"]["name"].is_string() {
                json!({"type":"function","function":{"name":choice["tool"]["name"]}})
            } else {
                return Err(invalid("Unsupported toolChoice"));
            };
        }
    }
    Ok((out, op.into(), "/v1/chat/completions"))
}
pub(super) fn stop(reason: &str) -> &'static str {
    match reason {
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        "content_filter" => "content_filtered",
        _ => "end_turn",
    }
}
pub(super) fn usage(v: &Value) -> Value {
    json!({"inputTokens":v["prompt_tokens"].as_u64().unwrap_or(0),"outputTokens":v["completion_tokens"].as_u64().unwrap_or(0),"totalTokens":v["total_tokens"].as_u64().unwrap_or(0),"cacheReadInputTokens":v["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0)})
}
pub(super) fn response(body: &Value) -> Value {
    let choice = &body["choices"][0];
    let m = &choice["message"];
    let mut content = Vec::new();
    if let Some(text) = m["content"].as_str() {
        content.push(json!({"text":text}));
    }
    for call in array(&m["tool_calls"]) {
        content.push(json!({"toolUse":{"toolUseId":call["id"],"name":call["function"]["name"],"input":serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(json!({}))}}));
    }
    json!({"output":{"message":{"role":"assistant","content":content}},"stopReason":stop(choice["finish_reason"].as_str().unwrap_or_default()),"usage":usage(&body["usage"]),"metrics":{"latencyMs":0}})
}
