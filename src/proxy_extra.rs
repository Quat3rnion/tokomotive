//! Small adapters for endpoints outside text generation.
use super::*;

pub(super) const COUNT_BODY_LIMIT: usize = 1024 * 1024;

pub(super) fn audio_upload(endpoint: Endpoint) -> bool {
    matches!(
        endpoint,
        Endpoint::AudioTranscription | Endpoint::AudioTranslation
    )
}

struct AudioField {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    data: Bytes,
}

pub(super) struct AudioUpload {
    fields: Vec<AudioField>,
    pub model: String,
    pub file_bytes: usize,
}

impl AudioUpload {
    pub async fn parse(headers: &HeaderMap, body: Bytes) -> anyhow::Result<Self> {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("multipart Content-Type is required"))?;
        let boundary = multer::parse_boundary(content_type)?;
        let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(body) });
        let mut multipart = multer::Multipart::new(stream, boundary);
        let mut fields = Vec::new();
        let mut model = None;
        let mut file_bytes = None;
        while let Some(field) = multipart.next_field().await? {
            anyhow::ensure!(fields.len() < 32, "too many multipart fields");
            let name = field
                .name()
                .ok_or_else(|| anyhow::anyhow!("field name is required"))?
                .to_owned();
            let filename = field.file_name().map(str::to_owned);
            let content_type = field.content_type().map(ToString::to_string);
            let data = field.bytes().await?;
            if name == "file" {
                anyhow::ensure!(
                    file_bytes.is_none() && !data.is_empty(),
                    "one non-empty file is required"
                );
                file_bytes = Some(data.len());
            } else {
                anyhow::ensure!(
                    data.len() <= 64 * 1024,
                    "multipart text field exceeds 64 KiB"
                );
                if name == "model" {
                    anyhow::ensure!(model.is_none(), "duplicate model field");
                    model = Some(std::str::from_utf8(&data)?.to_owned());
                }
                anyhow::ensure!(
                    name != "stream" || data != "true",
                    "audio upload streaming is not supported"
                );
            }
            fields.push(AudioField {
                name,
                filename,
                content_type,
                data,
            });
        }
        let model = model
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("model is required"))?;
        Ok(Self {
            fields,
            model,
            file_bytes: file_bytes.ok_or_else(|| anyhow::anyhow!("file is required"))?,
        })
    }

    pub fn form(&self, model: Option<&str>) -> reqwest::multipart::Form {
        let mut form = reqwest::multipart::Form::new();
        for field in &self.fields {
            let data = if field.name == "model" {
                Bytes::from(model.unwrap_or(&self.model).to_owned())
            } else {
                field.data.clone()
            };
            let mut part = reqwest::multipart::Part::stream_with_length(
                reqwest::Body::from(data.clone()),
                data.len() as u64,
            );
            if let Some(filename) = &field.filename {
                part = part.file_name(filename.clone());
            }
            if let Some(content_type) = &field.content_type {
                // The parser accepts only valid MIME types.
                part = part.mime_str(content_type).expect("parsed MIME type");
            }
            form = form.part(field.name.clone(), part);
        }
        form
    }
}

pub(super) fn audio_usage(v: Option<&Value>, file_bytes: usize) -> (Usage, bool) {
    let seconds = v
        .and_then(|v| v.pointer("/usage/seconds").or_else(|| v.get("duration")))
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0);
    let units = seconds.map_or_else(
        || file_bytes.div_ceil(16_000) as i64,
        |n| n.ceil().min(Usage::MAX_CLASS as f64) as i64,
    );
    (
        Usage {
            service_units: units,
            ..Default::default()
        },
        seconds.is_none(),
    )
}

pub(super) fn native_count(provider: &Provider) -> bool {
    provider.kind == ProviderKind::Anthropic
        && matches!(
            provider.auth,
            crate::provider::ProviderAuth::ApiKey | crate::provider::ProviderAuth::Claude
        )
}

pub(super) fn speech_response(
    ctx: Ctx,
    upstream: reqwest::Response,
    headers: HeaderMap,
    characters: i64,
) -> Response {
    let status = upstream.status();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let st = ctx.st.clone();
    st.spawn_tracked(async move {
        let mut stream = upstream.bytes_stream();
        let mut received = 0usize;
        let mut failed = false;
        let mut client_gone = false;
        let mut ttft = None;
        while let Some(item) = stream.next().await {
            let item = match item {
                Ok(chunk) if received.saturating_add(chunk.len()) <= MAX_UPSTREAM_BODY => {
                    received += chunk.len();
                    if ttft.is_none() && !chunk.is_empty() {
                        ttft = Some(ctx.started.elapsed().as_millis() as i64);
                    }
                    Ok(chunk)
                }
                Ok(_) => Err(std::io::Error::other("speech response exceeds 64 MiB")),
                Err(e) => Err(std::io::Error::other(e)),
            };
            failed = item.is_err();
            if !client_gone && tx.send(item).await.is_err() {
                client_gone = true;
            }
            if failed {
                break;
            }
        }
        ctx.finish(Outcome {
            status: if failed { 502 } else { status.as_u16() },
            model: None,
            usage: Usage {
                service_units: characters,
                ..Default::default()
            },
            reported_cost_micros: None,
            estimated: false,
            ttft_ms: ttft,
            client_aborted: client_gone,
        });
        drop(tx);
    });
    let mut response = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

pub(super) fn estimate_count(ctx: Ctx, body: &Value) -> Response {
    // Include tools and system text, but not routing and generation settings.
    let payload = json!({"messages":body.get("messages"),"system":body.get("system"),"tools":body.get("tools")});
    let tokens = payload.to_string().len().div_ceil(4);
    ctx.finish(Outcome {
        estimated: true,
        ..Outcome::failed(200)
    });
    (
        [("x-tokomotive-estimate", "true")],
        axum::Json(json!({"input_tokens":tokens})),
    )
        .into_response()
}
