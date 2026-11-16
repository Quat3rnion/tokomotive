use super::*;

fn validate_tools(body: &Value) -> Result<(), TranslateError> {
    if let Some(tools) = body.get("tools") {
        for tool in tools
            .as_array()
            .ok_or_else(|| invalid("tools must be an array"))?
        {
            for key in tool
                .as_object()
                .ok_or_else(|| invalid("Tool must be an object"))?
                .keys()
            {
                if key != "functionDeclarations" {
                    return Err(invalid(&format!("tools.{key} cannot be translated")));
                }
            }
            if !tool["functionDeclarations"].is_array() {
                return Err(invalid("Only functionDeclarations tools can be translated"));
            }
        }
    }
    if let Some(request) = body.get("generateContentRequest") {
        validate_tools(request)?;
    }
    Ok(())
}

pub(super) fn request(
    uri: &Uri,
    body: &Value,
) -> Result<(Value, String, &'static str), TranslateError> {
    for key in body
        .as_object()
        .ok_or_else(|| invalid("The request must be an object"))?
        .keys()
    {
        if ![
            "model",
            "contents",
            "systemInstruction",
            "tools",
            "toolConfig",
            "generationConfig",
            "safetySettings",
            "generateContentRequest",
            "content",
            "requests",
            "taskType",
            "title",
            "outputDimensionality",
        ]
        .contains(&key.as_str())
        {
            return Err(invalid(&format!("{key} cannot be translated")));
        }
    }
    validate_tools(body)?;
    let resource = uri
        .path()
        .strip_prefix("/v1beta/models/")
        .or_else(|| uri.path().strip_prefix("/v1/models/"))
        .ok_or_else(|| invalid("Unknown Gemini path."))?;
    let (model, op) = resource
        .rsplit_once(':')
        .ok_or_else(|| invalid("A model and operation are required."))?;
    let model = super::resource(model)?;
    if model.is_empty()
        || model.contains(['\\', '?', '#'])
        || model.split('/').any(|s| matches!(s, "." | ".." | ""))
    {
        return Err(invalid("Invalid model resource."));
    }
    if matches!(op, "embedContent" | "batchEmbedContents") {
        let requests = if op == "embedContent" {
            vec![body.clone()]
        } else {
            body["requests"]
                .as_array()
                .ok_or_else(|| invalid("requests must be an array"))?
                .clone()
        };
        if requests.is_empty() {
            return Err(invalid("requests must not be empty"));
        }
        let input = requests
            .iter()
            .map(|r| {
                if r.get("taskType").is_some()
                    || r.get("title").is_some()
                    || r.get("outputDimensionality").is_some()
                {
                    return Err(invalid(
                        "taskType, title and outputDimensionality cannot be translated",
                    ));
                }
                let mut text = String::new();
                for p in array(&r["content"]["parts"]) {
                    text.push_str(
                        p["text"]
                            .as_str()
                            .ok_or_else(|| invalid("Embedding content must contain text only"))?,
                    );
                }
                if text.is_empty() {
                    return Err(invalid("Embedding content must not be empty"));
                }
                Ok(json!(text))
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok((
            json!({"model":model,"input":input}),
            op.into(),
            "/v1/embeddings",
        ));
    }
    if !matches!(
        op,
        "generateContent" | "streamGenerateContent" | "countTokens"
    ) {
        return Err(invalid("Unsupported Gemini operation."));
    }
    let body = if op == "countTokens" {
        body.get("generateContentRequest").unwrap_or(body)
    } else {
        body
    };
    let mut out = json!({"model":model,"messages":[]});
    let mut messages = Vec::new();
    if let Some(system) = body.get("systemInstruction") {
        let mut text = String::new();
        for p in array(&system["parts"]) {
            text.push_str(
                p["text"]
                    .as_str()
                    .ok_or_else(|| invalid("systemInstruction must contain text only"))?,
            );
        }
        messages.push(json!({"role":"system","content":text}));
    }
    let mut calls = std::collections::HashMap::<String, std::collections::VecDeque<String>>::new();
    let mut next_call = 0;
    let contents = body["contents"]
        .as_array()
        .ok_or_else(|| invalid("contents must be an array"))?;
    for content in contents {
        let role = match content["role"].as_str().unwrap_or("user") {
            "user" => "user",
            "model" => "assistant",
            _ => return Err(invalid("Unsupported Gemini role")),
        };
        let mut parts = Vec::new();
        let mut tools = Vec::new();
        let mut results = Vec::new();
        for p in content["parts"]
            .as_array()
            .ok_or_else(|| invalid("parts must be an array"))?
        {
            if let Some(text) = p.get("text") {
                parts.push(json!({"type":"text","text":text}));
            } else if let Some(data) = p.get("inlineData") {
                let mime = data["mimeType"]
                    .as_str()
                    .filter(|s| s.starts_with("image/"))
                    .ok_or_else(|| invalid("Only inlineData images can be translated"))?;
                let data = data["data"]
                    .as_str()
                    .ok_or_else(|| invalid("inlineData.data must be base64"))?;
                parts.push(json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}")}}));
            } else if let Some(call) = p.get("functionCall") {
                let name = call["name"]
                    .as_str()
                    .ok_or_else(|| invalid("functionCall needs a name"))?;
                let id = call["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("call_{next_call}"));
                next_call += 1;
                calls.entry(name.into()).or_default().push_back(id.clone());
                let mut call_out = json!({"id":id,"type":"function","function":{"name":name,"arguments":call.get("args").cloned().unwrap_or(json!({})).to_string()}});
                if let Some(s) = p.get("thoughtSignature") {
                    call_out["thought_signature"] = s.clone();
                }
                tools.push(call_out);
            } else if let Some(result) = p.get("functionResponse") {
                let name = result["name"].as_str().unwrap_or_default();
                let id = if let Some(id) = result["id"].as_str() {
                    if let Some(queue) = calls.get_mut(name)
                        && let Some(index) = queue.iter().position(|v| v == id)
                    {
                        queue.remove(index);
                    }
                    id.to_owned()
                } else {
                    calls
                        .get_mut(name)
                        .and_then(std::collections::VecDeque::pop_front)
                        .ok_or_else(|| invalid("functionResponse has no matching functionCall"))?
                };
                results.push(json!({"role":"tool","tool_call_id":id,"content":result["response"].to_string()}));
            } else {
                return Err(invalid(
                    "Unsupported Gemini part; use text, inlineData, functionCall or functionResponse",
                ));
            }
        }
        // Chat requires tool results immediately after the assistant calls,
        // before any user text that shares the native result Content.
        messages.extend(results);
        if !parts.is_empty() || !tools.is_empty() {
            let mut message =
                json!({"role":role,"content":if parts.is_empty(){Value::Null}else{json!(parts)}});
            if !tools.is_empty() {
                message["tool_calls"] = json!(tools);
            }
            messages.push(message);
        }
    }
    out["messages"] = json!(messages);
    for (from, to) in [
        ("temperature", "temperature"),
        ("topP", "top_p"),
        ("topK", "top_k"),
        ("maxOutputTokens", "max_tokens"),
        ("stopSequences", "stop"),
    ] {
        if let Some(v) = body["generationConfig"].get(from) {
            out[to] = v.clone();
        }
    }
    if let Some(config) = body.get("generationConfig") {
        for key in config
            .as_object()
            .ok_or_else(|| invalid("generationConfig must be an object"))?
            .keys()
        {
            if ![
                "temperature",
                "topP",
                "topK",
                "maxOutputTokens",
                "stopSequences",
                "responseMimeType",
                "responseSchema",
                "responseJsonSchema",
                "candidateCount",
            ]
            .contains(&key.as_str())
            {
                return Err(invalid(&format!(
                    "generationConfig.{key} cannot be translated"
                )));
            }
        }
        if config
            .get("candidateCount")
            .is_some_and(|n| n.as_u64() != Some(1))
        {
            return Err(invalid("Only candidateCount = 1 is supported"));
        }
        let schema = config
            .get("responseSchema")
            .or_else(|| config.get("responseJsonSchema"));
        match config["responseMimeType"].as_str() {
            Some("application/json") => {
                out["response_format"] = if let Some(schema) = schema {
                    json!({"type":"json_schema","json_schema":{"name":"response","schema":schema_types(schema)}})
                } else {
                    json!({"type":"json_object"})
                }
            }
            None | Some("text/plain") if schema.is_none() => (),
            _ => {
                return Err(invalid(
                    "responseMimeType/responseSchema require application/json or text/plain without a schema",
                ));
            }
        }
    }
    let mut tools = Vec::new();
    for tool in array(&body["tools"]) {
        let declarations = tool["functionDeclarations"]
            .as_array()
            .ok_or_else(|| invalid("Only functionDeclarations tools can be translated"))?;
        for f in declarations {
            let mut f = f.clone();
            if let Some(p) = f.get("parameters") {
                f["parameters"] = schema_types(p);
            }
            tools.push(json!({"type":"function","function":f}));
        }
    }
    if !tools.is_empty() {
        out["tools"] = json!(tools);
    }
    if let Some(config) = body.get("toolConfig") {
        let f = &config["functionCallingConfig"];
        out["tool_choice"] = match f["mode"].as_str().unwrap_or("AUTO") {
            "AUTO" => json!("auto"),
            "NONE" => json!("none"),
            "ANY" => match array(&f["allowedFunctionNames"]) {
                [] => json!("required"),
                [name] => json!({"type":"function","function":{"name":name}}),
                _ => {
                    return Err(invalid(
                        "Only one allowedFunctionNames entry can be translated",
                    ));
                }
            },
            _ => return Err(invalid("Unsupported functionCallingConfig mode")),
        };
    }
    if op == "streamGenerateContent" {
        out["stream"] = json!(true);
        out["stream_options"] = json!({"include_usage":true});
    }
    Ok((
        out,
        op.into(),
        if op == "countTokens" {
            "/v1/messages/count_tokens"
        } else {
            "/v1/chat/completions"
        },
    ))
}
fn schema_types(value: &Value) -> Value {
    match value {
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        if k == "type" {
                            v.as_str()
                                .map(|s| json!(s.to_ascii_lowercase()))
                                .unwrap_or_else(|| v.clone())
                        } else {
                            schema_types(v)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(schema_types).collect()),
        _ => value.clone(),
    }
}
pub(super) fn aux_request(op: &str, body: &Value) -> Result<Value, TranslateError> {
    if op == "countTokens" {
        return crate::translate_gemini::request(Endpoint::ChatCompletions, body);
    }
    let inputs = body["input"]
        .as_array()
        .ok_or_else(|| invalid("Embedding input must be an array"))?;
    let requests=inputs.iter().map(|s|json!({"model":format!("models/{}",body["model"].as_str().unwrap_or_default()),"content":{"parts":[{"text":s}]}})).collect::<Vec<_>>();
    Ok(if op == "embedContent" {
        requests.first().cloned().unwrap_or_default()
    } else {
        json!({"requests":requests})
    })
}
pub(super) fn usage(u: &Value) -> Value {
    let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
    let output = u["completion_tokens"].as_u64().unwrap_or(0);
    let thoughts = u["completion_tokens_details"]["reasoning_tokens"]
        .as_u64()
        .unwrap_or(0)
        .min(output);
    json!({"promptTokenCount":prompt,"candidatesTokenCount":output-thoughts,"thoughtsTokenCount":thoughts,"totalTokenCount":u["total_tokens"].as_u64().unwrap_or_else(||prompt.saturating_add(output)),"cachedContentTokenCount":u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0).min(prompt)})
}
pub(super) fn response(op: &str, body: &Value) -> Value {
    match op {
        "countTokens" => json!({"totalTokens":body["input_tokens"]}),
        "embedContent" => json!({"embedding":{"values":body["data"][0]["embedding"]}}),
        "batchEmbedContents" => {
            json!({"embeddings":array(&body["data"]).iter().map(|e|json!({"values":e["embedding"]})).collect::<Vec<_>>()})
        }
        _ => {
            let choice = &body["choices"][0];
            let message = &choice["message"];
            let mut parts = Vec::new();
            if let Some(text) = message["content"].as_str() {
                parts.push(json!({"text":text}));
            }
            for call in array(&message["tool_calls"]) {
                parts.push(json!({"functionCall":{"id":call["id"],"name":call["function"]["name"],"args":serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(json!({}))}}));
            }
            json!({"candidates":[{"index":0,"content":{"role":"model","parts":parts},"finishReason":match choice["finish_reason"].as_str(){Some("length")=>"MAX_TOKENS",Some("content_filter")=>"SAFETY",_=>"STOP"}}],"usageMetadata":usage(&body["usage"])})
        }
    }
}
