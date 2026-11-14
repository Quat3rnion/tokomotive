//! Vertex REST routing. See docs/configuration.md for the source URLs.
use crate::provider::ProviderKind;
use anyhow::{Result, bail};

pub fn validate_project(project: &str) -> Result<()> {
    if !(6..=30).contains(&project.len())
        || !project.starts_with(|c: char| c.is_ascii_lowercase())
        || !project.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !project
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!(
            "Project ID must have 6 to 30 lowercase letters, digits or hyphens, start with a letter and end with a letter or digit"
        );
    }
    Ok(())
}

pub fn base_url(region: &str) -> Result<String> {
    if region == "global" {
        return Ok("https://aiplatform.googleapis.com".into());
    }
    let parts: Vec<_> = region.split('-').collect();
    if parts.len() != 2
        || !(2..=20).contains(&parts[0].len())
        || !parts[0].bytes().all(|b| b.is_ascii_lowercase())
        || !(2..=20).contains(&parts[1].len())
        || !parts[1].starts_with(|c: char| c.is_ascii_lowercase())
        || !parts[1].ends_with(|c: char| c.is_ascii_digit())
        || !parts[1]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        bail!("Enter a region such as us-central1 or global");
    }
    Ok(format!("https://{region}-aiplatform.googleapis.com"))
}

pub fn model(model: &str) -> Result<(ProviderKind, &str)> {
    let (kind, id) = if let Some(id) = model.strip_prefix("anthropic/") {
        (ProviderKind::Anthropic, id)
    } else if let Some(id) = model.strip_prefix("google/") {
        (ProviderKind::Gemini, id)
    } else if model.starts_with("claude-") {
        (ProviderKind::Anthropic, model)
    } else {
        (ProviderKind::Gemini, model)
    };
    if id.is_empty()
        || id.len() > 256
        || !id.starts_with(|c: char| c.is_ascii_alphanumeric())
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'))
        || id == "."
        || id == ".."
    {
        bail!(
            "Vertex model must be a single model ID, with an optional google/ or anthropic/ prefix"
        );
    }
    Ok((kind, id))
}

pub fn model_path(project: &str, region: &str, name: &str, stream: bool) -> Result<String> {
    validate_project(project)?;
    base_url(region)?;
    let (kind, id) = model(name)?;
    let (publisher, action) = if kind == ProviderKind::Anthropic {
        (
            "anthropic",
            if stream {
                "streamRawPredict"
            } else {
                "rawPredict"
            },
        )
    } else {
        (
            "google",
            if stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            },
        )
    };
    Ok(format!(
        "/v1/projects/{project}/locations/{region}/publishers/{publisher}/models/{id}:{action}"
    ))
}
