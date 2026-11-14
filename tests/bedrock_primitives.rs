use std::collections::BTreeMap;
use tokomotive::{
    eventstream::Decoder,
    sigv4::{Credentials, sign},
};

// AWS-published vectors: awslabs/aws-c-auth/tests/aws-signing-test-suite/v4.
#[test]
fn aws_official_vanilla_vectors() {
    let creds = Credentials::new(
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        None,
    );
    for (method, expected) in [
        (
            "GET",
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
        ),
        (
            "POST",
            "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
        ),
    ] {
        let headers = BTreeMap::from([
            ("host".into(), "example.amazonaws.com".into()),
            ("x-amz-date".into(), "20150830T123600Z".into()),
        ]);
        let signed = sign(
            &creds,
            "us-east-1",
            "service",
            method,
            "/",
            "",
            &headers,
            b"",
        )
        .unwrap();
        assert!(
            signed.authorization.ends_with(expected),
            "{}",
            signed.authorization
        );
    }
}

#[test]
fn decoder_rejects_oversized_and_truncated_frames() {
    let mut d = Decoder::default();
    assert!(d.feed(&[1]).unwrap().is_empty());
    assert!(d.finish().is_err());
    let mut prelude = Vec::from(17_000_000u32.to_be_bytes());
    prelude.extend(0u32.to_be_bytes());
    prelude.extend(0u32.to_be_bytes());
    assert!(Decoder::default().feed(&prelude).is_err());
}

#[test]
fn aws_official_form_vector_and_redacted_credentials() {
    let creds = Credentials::new(
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        Some("token".into()),
    );
    let h = BTreeMap::from([
        ("host".into(), "example.amazonaws.com".into()),
        ("x-amz-date".into(), "20150830T123600Z".into()),
        (
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        ),
        ("content-length".into(), "13".into()),
        (
            "x-amz-content-sha256".into(),
            "9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e".into(),
        ),
    ]);
    let s = sign(
        &creds,
        "us-east-1",
        "service",
        "POST",
        "/",
        "",
        &h,
        b"Param1=value1",
    )
    .unwrap();
    assert!(
        s.authorization
            .ends_with("d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b")
    );
    assert!(!format!("{creds:?}").contains("AKID"));
    assert!(!format!("{creds:?}").contains("EXAMPLEKEY"));
    for region in [
        "us-east-1.evil",
        "us-east-1/evil",
        "US-east-1",
        "us-east-1\r\nx: y",
        "https://us-east-1",
        "us--1",
        "us-east-10",
    ] {
        assert!(tokomotive::sigv4::host(region).is_err());
    }
}

pub fn frame(event: &str, payload: &[u8]) -> Vec<u8> {
    let mut headers = Vec::new();
    for (n, v) in [(":message-type", "event"), (":event-type", event)] {
        headers.push(n.len() as u8);
        headers.extend(n.as_bytes());
        headers.push(7);
        headers.extend((v.len() as u16).to_be_bytes());
        headers.extend(v.as_bytes());
    }
    let total = 16 + headers.len() + payload.len();
    let mut out = Vec::from((total as u32).to_be_bytes());
    out.extend((headers.len() as u32).to_be_bytes());
    out.extend(crc32fast::hash(&out).to_be_bytes());
    out.extend(headers);
    out.extend(payload);
    out.extend(crc32fast::hash(&out).to_be_bytes());
    out
}
#[test]
fn decoder_checks_both_crcs_and_all_split_positions() {
    let f = frame("messageStart", b"{\"role\":\"assistant\"}");
    for split in 0..=f.len() {
        let mut d = Decoder::default();
        let mut frames = d.feed(&f[..split]).unwrap();
        frames.extend(d.feed(&f[split..]).unwrap());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].headers[":event-type"], "messageStart");
        assert_eq!(frames[0].payload, b"{\"role\":\"assistant\"}");
        assert!(d.finish().is_ok());
    }
    for pos in [8, f.len() - 1] {
        let mut bad = f.clone();
        bad[pos] ^= 1;
        assert!(Decoder::default().feed(&bad).is_err());
    }
    for end in 1..f.len() {
        let mut d = Decoder::default();
        d.feed(&f[..end]).unwrap();
        assert!(d.finish().is_err());
    }
}

#[test]
fn bedrock_signed_request_uses_encoded_model_and_session_token() {
    let credentials = Credentials::new(
        "AKIDEXAMPLE",
        "secret-never-transmitted",
        Some("session-token".into()),
    );
    let path = tokomotive::translate_bedrock::path(
        "arn:aws:bedrock:us-east-1:123456789012:inference-profile/test",
        false,
    )
    .unwrap();
    let r = tokomotive::sigv4::request(
        &reqwest::Client::new(),
        &credentials,
        "us-east-1",
        &path,
        b"{}",
        "20150830T123600Z",
    )
    .unwrap()
    .body("{}")
    .build()
    .unwrap();
    assert_eq!(
        r.url().host_str(),
        Some("bedrock-runtime.us-east-1.amazonaws.com")
    );
    assert!(r.url().path().contains("%2Ftest"));
    assert_eq!(r.headers()["x-amz-date"], "20150830T123600Z");
    assert_eq!(r.headers()["x-amz-security-token"], "session-token");
    assert_eq!(
        r.headers()["x-amz-content-sha256"],
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
    assert!(
        r.headers()["authorization"]
            .to_str()
            .unwrap()
            .contains("/us-east-1/bedrock/aws4_request")
    );
    assert!(!format!("{r:?}").contains("secret-never-transmitted"));
    assert!(!format!("{r:?}").contains("session-token"));
    for base in [
        "https://bedrock-runtime.us-east-1.amazonaws.com.evil",
        "http://bedrock-runtime.us-east-1.amazonaws.com",
        "https://bedrock-runtime.us-east-1.amazonaws.com/path",
        "https://evil@bedrock-runtime.us-east-1.amazonaws.com",
    ] {
        assert!(tokomotive::sigv4::region_from_base(base).is_err());
    }
}
