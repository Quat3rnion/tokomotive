use llm_accounting::translate::{Direction, translate_request};
use serde_json::json;

#[test]
fn tool_result_images_follow_all_consecutive_tool_replies() {
    let image = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}});
    let out = translate_request(Direction::MessagesToChat, &json!({"messages":[
        {"role":"assistant","content":[
            {"type":"tool_use","id":"a","name":"lookup","input":{}},
            {"type":"tool_use","id":"b","name":"lookup","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":[{"type":"text","text":"first"},image]}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"b","content":[image]}]},
        {"role":"assistant","content":"Done"}
    ]})).unwrap();
    let messages = out["messages"].as_array().unwrap();
    assert_eq!(messages[1]["content"], "first", "tool replies must be text-only");
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["content"], "");
    assert_eq!(messages[3]["role"], "user");
    assert_eq!(messages[3]["content"][0]["text"], "Images from tool result a:");
    assert_eq!(messages[3]["content"][1]["type"], "image_url");
    assert_eq!(messages[3]["content"][2]["text"], "Images from tool result b:");
    assert_eq!(messages[3]["content"][3]["type"], "image_url");
    assert_eq!(messages[4]["content"], "Done");
}

#[test]
fn empty_assistant_text_keeps_only_tool_use() {
    for content in [json!(""), json!([{"type":"text","text":""}])] {
        let out = translate_request(Direction::ChatToMessages, &json!({"messages":[{
            "role":"assistant","content":content,"tool_calls":[{
                "id":"call1","function":{"name":"lookup","arguments":"{}"}
            }]
        }]})).unwrap();
        assert_eq!(out["messages"][0]["content"].as_array().unwrap().len(), 1,
            "empty Anthropic text block must be omitted");
        assert_eq!(out["messages"][0]["content"][0]["type"], "tool_use");
    }
}

#[test]
fn null_chat_limits_use_non_null_fallback_or_default() {
    for (limits, expected) in [
        (json!({"max_tokens":null}), 4096),
        (json!({"max_completion_tokens":null}), 4096),
        (json!({"max_completion_tokens":null,"max_tokens":123}), 123),
    ] {
        let mut body = limits;
        body["messages"] = json!([{"role":"user","content":"Hi"}]);
        let out = translate_request(Direction::ChatToMessages, &body).unwrap();
        assert_eq!(out["max_tokens"], expected);
    }
}
